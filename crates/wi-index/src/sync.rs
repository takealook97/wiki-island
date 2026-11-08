use crate::{
    Error, Index, Query, Result, SearchMode, SearchResult, content_hash, make_hit, now, query,
    script::build_script_ngrams,
};
use ignore::WalkBuilder;
use rayon::prelude::*;
use rusqlite::{Connection, Transaction, params, params_from_iter, types::Value as SqlValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    time::{Instant, UNIX_EPOCH},
};
use wi_note::{Note, VaultConfig, generate_permalink};

#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncReport {
    pub discovered: usize,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub hashed: usize,
    pub parsed: usize,
    pub elapsed_ms: u128,
    pub index_as_of: Option<String>,
    pub diagnostics: Vec<IndexDiagnostic>,
    pub skipped: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDiagnostic {
    pub rel_path: PathBuf,
    pub message: String,
}
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    mtime_ns: String,
    size: u64,
}
#[derive(Clone)]
struct Existing {
    stamp: Stamp,
    hash: String,
}
struct Prepared {
    path: String,
    stamp: Stamp,
    hash: String,
    note: Option<Note>,
}

pub(crate) fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.to_str().is_none()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || path.to_string_lossy().contains('\\')
    {
        return Err(Error::InvalidPath(path.into()));
    }
    Ok(())
}
fn stamp(path: &Path) -> Result<Stamp> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::InvalidPath(path.into()));
    }
    let time = metadata.modified()?.duration_since(UNIX_EPOCH).map_or_else(
        |e| format!("-{}", e.duration().as_nanos()),
        |d| d.as_nanos().to_string(),
    );
    Ok(Stamp {
        mtime_ns: time,
        size: metadata.len(),
    })
}
fn markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

const DEFAULT_MAX_NOTE_BYTES: u64 = 8 * 1024 * 1024;
struct Settings {
    ignored: Vec<PathBuf>,
    max_note_bytes: u64,
}
fn settings(root: &Path) -> Result<Settings> {
    let mut settings = Settings {
        ignored: vec![PathBuf::from(".wiki-island/local")],
        max_note_bytes: DEFAULT_MAX_NOTE_BYTES,
    };
    let path = root.join(wi_note::VAULT_CONFIG_PATH);
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(settings),
        Err(e) => return Err(e.into()),
    };
    let config = VaultConfig::parse(&text)?;
    settings.max_note_bytes = config.max_note_bytes();
    let invalid = || {
        Error::Config(wi_note::ConfigError::Field(
            "ignore_dirs must be an array of safe relative directories".into(),
        ))
    };
    if let Some(value) = config.get("ignore_dirs") {
        for item in value.as_array().ok_or_else(invalid)? {
            let path = PathBuf::from(item.as_str().ok_or_else(invalid)?);
            validate_relative(&path).map_err(|_| invalid())?;
            settings.ignored.push(path);
        }
    }
    Ok(settings)
}
fn skip(report: &mut SyncReport, path: impl Into<PathBuf>, error: impl std::fmt::Display) {
    let path: PathBuf = path.into();
    // Diagnostic paths must remain serializable even for unsupported byte names.
    let path = PathBuf::from(path.to_string_lossy().into_owned());
    report.skipped.push(path.clone());
    report.diagnostics.push(IndexDiagnostic {
        rel_path: path,
        message: error.to_string(),
    });
}
/// Replace the stored `skipped` and `diagnostics` entries of the `affected`
/// paths with a targeted update's own, so search results stop reporting a
/// repaired file as skipped (and start reporting a newly broken one) before
/// the next full walk. Other paths keep their last-walk entries.
fn refresh_walk_meta(
    tx: &rusqlite::Transaction<'_>,
    affected: &[String],
    skipped: &[PathBuf],
    diagnostics: &[IndexDiagnostic],
) -> Result<()> {
    use rusqlite::OptionalExtension;
    let touched: BTreeSet<&str> = affected.iter().map(String::as_str).collect();
    let is_touched = |p: &Path| touched.contains(p.to_string_lossy().as_ref());
    let read = |key: &str| -> Result<Option<String>> {
        Ok(tx
            .query_row("SELECT value FROM index_meta WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    };
    let write = |key: &str, value: String| -> Result<()> {
        tx.execute(
            "INSERT INTO index_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [key, value.as_str()],
        )?;
        Ok(())
    };
    if let Some(stored) = read("skipped")? {
        let stored: Vec<PathBuf> = serde_json::from_str(&stored)?;
        let kept: Vec<PathBuf> = stored
            .into_iter()
            .filter(|p| !is_touched(p))
            .chain(skipped.iter().cloned())
            .collect();
        write("skipped", serde_json::to_string(&kept)?)?;
    }
    if let Some(stored) = read("diagnostics")? {
        let stored: Vec<IndexDiagnostic> = serde_json::from_str(&stored)?;
        let kept: Vec<IndexDiagnostic> = stored
            .into_iter()
            .filter(|d| !is_touched(&d.rel_path))
            .chain(diagnostics.iter().cloned())
            .collect();
        write("diagnostics", serde_json::to_string(&kept)?)?;
    }
    Ok(())
}
/// Top-level folders of other applications sharing the vault (Obsidian
/// settings and trash): never indexed.
const FOREIGN_DIRS: &[&str] = &[".obsidian", ".trash"];

fn reserved(path: &Path, ignored: &[PathBuf]) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
        || path.components().next().is_some_and(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|s| FOREIGN_DIRS.contains(&s.to_lowercase().as_str()))
        })
        || ignored.iter().any(|dir| path.starts_with(dir))
        || wi_note::is_unindexed_engine_path(&path.to_string_lossy())
}
fn walk(root: &Path, ignored: Vec<PathBuf>, report: &mut SyncReport) -> BTreeMap<String, Stamp> {
    let root_owned = root.to_path_buf();
    let exclude = exclude_matcher(root);
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .require_git(false)
        .filter_entry(move |entry| {
            entry
                .path()
                .strip_prefix(&root_owned)
                .is_ok_and(|rel| !reserved(rel, &ignored))
                && !exclude.as_ref().is_some_and(|m| {
                    m.matched(entry.path(), entry.file_type().is_some_and(|t| t.is_dir()))
                        .is_ignore()
                })
        });
    let mut files = BTreeMap::new();
    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                // An ignore walker error may hide a directory; retain old rows below it.
                skip(report, PathBuf::new(), e);
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) || !markdown(entry.path()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let checked = validate_relative(relative).and_then(|()| stamp(entry.path()));
        match checked {
            Ok(stamp) => {
                files.insert(relative.to_string_lossy().into_owned(), stamp);
            }
            Err(e) => skip(report, relative, e),
        }
    }
    files
}
fn allowed(root: &Path, path: &Path, ignored: &[PathBuf]) -> Result<bool> {
    if reserved(path, ignored) || !markdown(path) {
        return Ok(false);
    }
    let mut ancestor = root.to_path_buf();
    for component in path.components() {
        ancestor.push(component);
        if fs::symlink_metadata(&ancestor).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Error::InvalidPath(path.into()));
        }
    }
    let (excluded, mut errors) = ignored_with_errors(root, path);
    if !errors.is_empty() {
        return Err(errors.swap_remove(0).into());
    }
    Ok(!excluded)
}

/// Extra exclude files (a folder-mode vault's `<gitdir>/info/exclude`),
/// keyed by canonical vault root. The vault registers its file on open.
static EXCLUDES: std::sync::Mutex<BTreeMap<PathBuf, PathBuf>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Register (or clear) the git exclude file whose rules apply to the vault
/// at `root` below its `.ignore` and `.gitignore` files. A git-mode vault
/// keeps its rules in `.gitignore` and registers nothing.
pub fn set_exclude_file(root: &Path, file: Option<&Path>) {
    let key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut map = EXCLUDES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match file {
        Some(file) => {
            map.insert(key, file.to_path_buf());
        }
        None => {
            map.remove(&key);
        }
    }
}

/// The matcher of the exclude file registered for `root`, rooted at `root`.
pub fn exclude_matcher(root: &Path) -> Option<ignore::gitignore::Gitignore> {
    let file = {
        let map = EXCLUDES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(root)
            .or_else(|| root.canonicalize().ok().and_then(|c| map.get(&c)))
            .cloned()?
    };
    if !file.is_file() {
        return None;
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    builder.add(file);
    builder.build().ok()
}

/// Whether the `.ignore` and `.gitignore` files of `root` and of the
/// directories above `rel` exclude the vault-relative `rel`, decided the
/// way the vault walk decides: an excluded directory hides everything
/// below it, deeper files win over shallower ones, and `.ignore` wins over
/// `.gitignore`. Scans, file events and the index share this one rule.
/// Rules that parse apply even when a file is partly invalid.
pub fn ignored(root: &Path, rel: &Path) -> bool {
    ignored_with_errors(root, rel).0
}

/// [`ignored`] for a directory: the last component of `rel` is matched as
/// a directory too, so a `name/` pattern excludes it. Walkers ask this
/// before reading a directory, so an excluded (even unreadable) one is
/// pruned without being opened.
pub fn ignored_dir(root: &Path, rel: &Path) -> bool {
    ignored_kind(root, rel, true).0
}

/// [`ignored`], also returning the errors of unreadable or partly invalid
/// ignore files.
pub fn ignored_with_errors(root: &Path, rel: &Path) -> (bool, Vec<ignore::Error>) {
    ignored_kind(root, rel, false)
}

/// The shared rule; `dir` says whether the last component of `rel` is a
/// directory (every earlier component is one).
fn ignored_kind(root: &Path, rel: &Path, dir: bool) -> (bool, Vec<ignore::Error>) {
    use ignore::{Match, gitignore::Gitignore};
    let mut errors = Vec::new();
    let mut load = |dir: &Path, name: &str| -> Option<Gitignore> {
        let file = dir.join(name);
        if !file.is_file() {
            return None;
        }
        let mut builder = ignore::gitignore::GitignoreBuilder::new(dir);
        if let Some(error) = builder.add(file) {
            errors.push(error);
        }
        match builder.build() {
            Ok(matcher) => Some(matcher),
            Err(error) => {
                errors.push(error);
                None
            }
        }
    };
    let parts: Vec<_> = rel.components().collect();
    let exclude = exclude_matcher(root);
    // Matchers of the directories from the root down, by file kind.
    let mut dot_ignore: Vec<Gitignore> = Vec::new();
    let mut git_ignore: Vec<Gitignore> = Vec::new();
    let mut path = root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        dot_ignore.extend(load(&path, ".ignore"));
        git_ignore.extend(load(&path, ".gitignore"));
        path.push(part);
        let is_dir = dir || i + 1 < parts.len();
        let decide = |matchers: &[Gitignore]| {
            matchers
                .iter()
                .rev()
                .find_map(|m| match m.matched(&path, is_dir) {
                    Match::None => None,
                    found => Some(found.is_ignore()),
                })
        };
        if decide(&dot_ignore)
            .or_else(|| decide(&git_ignore))
            .or_else(|| decide(exclude.as_slice()))
            == Some(true)
        {
            return (true, errors);
        }
    }
    (false, errors)
}
fn prepare(
    root: &Path,
    path: &str,
    expected: &Stamp,
    old: Option<&Existing>,
    max_bytes: u64,
) -> Result<Prepared> {
    let full = root.join(path);
    // Checking each ancestor rejects a symlink introduced before a targeted update.
    let mut ancestor = root.to_path_buf();
    for part in Path::new(path).components() {
        ancestor.push(part);
        if fs::symlink_metadata(&ancestor)?.file_type().is_symlink() {
            return Err(Error::InvalidPath(full));
        }
    }
    let before = stamp(&full)?;
    if &before != expected {
        return Err(Error::ChangedDuringRead(full));
    }
    if before.size > max_bytes {
        return Err(Error::Oversize(full));
    }
    // Bound the read itself as well: a writer can grow a file after the stat.
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(&full)?
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(Error::Oversize(full));
    }
    if stamp(&full)? != before || bytes.len() as u64 != before.size {
        return Err(Error::ChangedDuringRead(full));
    }
    let hash = content_hash(&bytes);
    let note = if old.is_some_and(|e| e.hash == hash) {
        None
    } else {
        Some(Note::parse(&bytes)?)
    };
    Ok(Prepared {
        path: path.into(),
        stamp: before,
        hash,
        note,
    })
}

// Limit bytes retained by a parallel batch before parsing projections.
fn batch_size(sizes: impl Iterator<Item = u64>) -> usize {
    (32 * 1024 * 1024 / sizes.max().unwrap_or(1).max(1)).clamp(1, 128) as usize
}

fn prepare_retry(
    root: &Path,
    path: &str,
    expected: &Stamp,
    old: Option<&Existing>,
    max_bytes: u64,
) -> Result<Prepared> {
    match prepare(root, path, expected, old, max_bytes) {
        Err(Error::ChangedDuringRead(_)) => {
            prepare(root, path, &stamp(&root.join(path))?, old, max_bytes)
        }
        result => result,
    }
}

impl Index {
    /// Complete stat-first walk; no file bytes are read when mtime and size match.
    pub fn sync(&mut self, vault_root: impl AsRef<Path>) -> Result<SyncReport> {
        if fs::canonicalize(vault_root)? != self.vault_root {
            return Err(Error::VaultMismatch);
        }
        self.sync_inner(false, false)
    }
    /// Reparse canonical files atomically, retaining previous rows for skipped files.
    /// Database/configuration failure keeps the previous index intact.
    pub fn rebuild(&mut self) -> Result<SyncReport> {
        self.sync_inner(true, true)
    }
    /// Optional caller-scheduled hash sweep. No schedule is installed by the library.
    /// Unlike rebuild, unchanged hashes reuse parsed rows.
    pub fn reverify(&mut self) -> Result<SyncReport> {
        self.sync_inner(false, true)
    }
    fn existing(&self) -> Result<BTreeMap<String, Existing>> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT path,mtime_ns,size,content_hash FROM files")?;
        Ok(statement
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    Existing {
                        stamp: Stamp {
                            mtime_ns: r.get(1)?,
                            size: r.get(2)?,
                        },
                        hash: r.get(3)?,
                    },
                ))
            })?
            .collect::<std::result::Result<_, _>>()?)
    }
    fn sync_inner(&mut self, force: bool, hash_all: bool) -> Result<SyncReport> {
        let start = Instant::now();
        let as_of = now();
        let settings = settings(&self.vault_root)?;
        let mut report = SyncReport::default();
        let files = walk(&self.vault_root, settings.ignored, &mut report);
        let existing = self.existing()?;
        let unknown_walk_error = report.skipped.iter().any(|p| p.as_os_str().is_empty());
        let skipped: BTreeSet<_> = report.skipped.iter().collect();
        let removed: Vec<_> = existing
            .keys()
            .filter(|p| {
                !files.contains_key(*p)
                    && !unknown_walk_error
                    && !skipped.contains(&PathBuf::from(p.as_str()))
            })
            .cloned()
            .collect();
        let changed: Vec<_> = files
            .iter()
            .filter(|(p, s)| {
                hash_all
                    || s.size > settings.max_note_bytes
                    || existing.get(*p).is_none_or(|e| &e.stamp != *s)
            })
            .collect();
        report = SyncReport {
            discovered: files.len(),
            removed: removed.len(),
            unchanged: files.len() - changed.len(),
            index_as_of: Some(as_of.clone()),
            ..report
        };
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for path in removed {
            remove(&tx, &path)?;
        }
        let batch_size = batch_size(changed.iter().map(|(_, stamp)| stamp.size));
        for batch in changed.chunks(batch_size) {
            let prepared: Vec<_> = batch
                .par_iter()
                .map(|(path, s)| {
                    prepare_retry(
                        &self.vault_root,
                        path,
                        s,
                        if force { None } else { existing.get(*path) },
                        settings.max_note_bytes,
                    )
                })
                .collect::<Vec<_>>();
            for ((path, _), item) in batch.iter().zip(prepared) {
                let item = match item {
                    Ok(item) => item,
                    Err(e) => {
                        skip(&mut report, PathBuf::from(path), e);
                        continue;
                    }
                };
                report.hashed += 1;
                if let Some(note) = &item.note {
                    report.parsed += 1;
                    report
                        .diagnostics
                        .extend(note.diagnostics().iter().map(|d| IndexDiagnostic {
                            rel_path: PathBuf::from(&item.path),
                            message: d.message.clone(),
                        }));
                }
                if existing.contains_key(&item.path) {
                    report.updated += 1;
                } else {
                    report.added += 1;
                }
                store(&tx, item)?;
            }
        }
        if force || report.parsed > 0 || report.removed > 0 {
            resolve(&tx)?;
        }
        tx.execute("INSERT INTO index_meta(key,value) VALUES('skipped',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [serde_json::to_string(&report.skipped)?])?;
        tx.execute("INSERT INTO index_meta(key,value) VALUES('diagnostics',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [serde_json::to_string(&report.diagnostics)?])?;
        tx.execute("INSERT INTO index_meta(key,value) VALUES('index_as_of',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&as_of])?;
        tx.commit()?;
        report.elapsed_ms = start.elapsed().as_millis();
        Ok(report)
    }
    /// Watcher/write-path update. Does not advance the complete stat-walk watermark.
    /// Missing and newly ignored paths are removed. Paths are validated before any writes.
    pub fn update_paths<P: AsRef<Path>>(&mut self, paths: &[P]) -> Result<SyncReport> {
        let start = Instant::now();
        let settings = settings(&self.vault_root)?;
        let mut report = SyncReport::default();
        let mut updates = BTreeMap::new();
        let mut removals = BTreeSet::new();
        for path in paths {
            let path = path.as_ref();
            validate_relative(path)?;
            let text = path.to_string_lossy().into_owned();
            if !allowed(&self.vault_root, path, &settings.ignored)? {
                removals.insert(text);
                continue;
            }
            match stamp(&self.vault_root.join(path)) {
                Ok(s) => {
                    updates.insert(text, s);
                }
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    removals.insert(text);
                }
                Err(e) => skip(&mut report, path, e),
            }
        }
        let mut existing = BTreeMap::new();
        let mut lookup = self
            .connection
            .prepare_cached("SELECT mtime_ns,size,content_hash FROM files WHERE path=?1")?;
        for path in updates.keys().chain(removals.iter()) {
            use rusqlite::OptionalExtension;
            if let Some(record) = lookup
                .query_row([path], |r| {
                    Ok(Existing {
                        stamp: Stamp {
                            mtime_ns: r.get(0)?,
                            size: r.get(1)?,
                        },
                        hash: r.get(2)?,
                    })
                })
                .optional()?
            {
                existing.insert(path.clone(), record);
            }
        }
        drop(lookup);
        report = SyncReport {
            discovered: updates.len(),
            index_as_of: self.index_as_of()?,
            ..report
        };
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let affected: Vec<_> = updates.keys().chain(removals.iter()).cloned().collect();
        let mut identities = affected_names(&tx, &affected)?;
        for path in removals {
            if existing.contains_key(&path) {
                remove(&tx, &path)?;
                report.removed += 1;
            }
        }
        // Targeted updates always read/hash, even with an unchanged stat (an acknowledged write).
        let updates: Vec<_> = updates.into_iter().collect();
        let batch_size = batch_size(updates.iter().map(|(_, stamp)| stamp.size));
        for batch in updates.chunks(batch_size) {
            let prepared: Vec<_> = batch
                .par_iter()
                .map(|(p, s)| {
                    prepare_retry(
                        &self.vault_root,
                        p,
                        s,
                        existing.get(p),
                        settings.max_note_bytes,
                    )
                })
                .collect::<Vec<_>>();
            for ((path, _), item) in batch.iter().zip(prepared) {
                let item = match item {
                    Ok(item) => item,
                    Err(e) => {
                        skip(&mut report, path, e);
                        continue;
                    }
                };
                report.hashed += 1;
                if let Some(note) = &item.note {
                    report.parsed += 1;
                    report
                        .diagnostics
                        .extend(note.diagnostics().iter().map(|d| IndexDiagnostic {
                            rel_path: PathBuf::from(&item.path),
                            message: d.message.clone(),
                        }));
                }
                if existing.contains_key(&item.path) {
                    report.updated += 1;
                } else {
                    report.added += 1;
                }
                store(&tx, item)?;
            }
        }
        if report.parsed > 0 || report.removed > 0 {
            identities.extend(affected_names(&tx, &affected)?);
            resolve_affected(&tx, &affected, &identities)?;
        }
        refresh_walk_meta(&tx, &affected, &report.skipped, &report.diagnostics)?;
        tx.commit()?;
        report.elapsed_ms = start.elapsed().as_millis();
        Ok(report)
    }
    pub fn remove_paths<P: AsRef<Path>>(&mut self, paths: &[P]) -> Result<usize> {
        for path in paths {
            validate_relative(path.as_ref())?;
        }
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let affected: Vec<_> = paths
            .iter()
            .map(|p| p.as_ref().to_string_lossy().into_owned())
            .collect();
        let identities = affected_names(&tx, &affected)?;
        let mut count = 0;
        for path in paths {
            count += remove(&tx, &path.as_ref().to_string_lossy())?;
        }
        resolve_affected(&tx, &affected, &identities)?;
        refresh_walk_meta(&tx, &affected, &[], &[])?;
        tx.commit()?;
        Ok(count)
    }
}
fn remove(tx: &Transaction<'_>, path: &str) -> Result<usize> {
    tx.prepare_cached(
        "DELETE FROM search_fts WHERE rowid IN (SELECT rowid FROM notes WHERE path=?1)",
    )?
    .execute([path])?;
    tx.prepare_cached("DELETE FROM lifecycle_snapshots WHERE path=?1")?
        .execute([path])?;
    Ok(tx
        .prepare_cached("DELETE FROM files WHERE path=?1")?
        .execute([path])?)
}
fn metadata(note: &Note) -> Value {
    let mut map = Map::new();
    // Malformed frontmatter is plain text and must never become authoritative metadata.
    if let Some(fm) = note
        .frontmatter()
        .filter(|fm| note.body_start() == fm.byte_range.end)
    {
        for entry in &fm.entries {
            map.insert(entry.key.clone(), entry.value.clone());
        }
    }
    Value::Object(map)
}
fn names(path: &str, metadata: &Value) -> (String, String) {
    let permalink = metadata
        .get("permalink")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| generate_permalink(path));
    let title = metadata
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            Path::new(path)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    (permalink, title)
}
fn store(tx: &Transaction<'_>, item: Prepared) -> Result<()> {
    let size =
        i64::try_from(item.stamp.size).map_err(|_| Error::InvalidPath(item.path.clone().into()))?;
    if item.note.is_none() {
        tx.prepare_cached("UPDATE files SET mtime_ns=?2,size=?3 WHERE path=?1")?
            .execute(params![item.path, item.stamp.mtime_ns, size])?;
        return Ok(());
    }
    remove(tx, &item.path)?;
    tx.prepare_cached("INSERT INTO files(path,mtime_ns,size,content_hash) VALUES(?1,?2,?3,?4)")?
        .execute(params![item.path, item.stamp.mtime_ns, size, item.hash])?;
    if let Some(note) = item.note {
        let metadata = metadata(&note);
        let (permalink, title) = names(&item.path, &metadata);
        let note_type = metadata
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("note");
        tx.prepare_cached("INSERT INTO notes(path,permalink,title,note_type,content,metadata) VALUES(?1,?2,?3,?4,?5,?6)")?.execute(params![item.path,permalink,title,note_type,note.body(),metadata.to_string()])?;
        let rowid = tx.last_insert_rowid();
        tx.prepare_cached("INSERT INTO search_fts(rowid,title,content,script_ngrams,permalink) VALUES(?1,?2,?3,?4,?5)")?.execute(params![rowid,title,note.body(),build_script_ngrams(&[&title,note.body()]),permalink])?;
        // Every lifecycle note gets a row (the cold listing reads only this
        // table) holding its engine snapshot of these bytes when that
        // round-trips, so a cold current read need not re-parse each file.
        if wi_lifecycle::LIFECYCLE_TYPES.contains(&note_type) {
            let snapshot =
                wi_lifecycle::NoteSnapshot::from_note(item.path.as_str(), &note).materialize();
            tx.prepare_cached("INSERT OR REPLACE INTO lifecycle_snapshots(path,content_hash,format,snapshot) VALUES(?1,?2,?3,?4)")?
                .execute(params![item.path, item.hash, wi_lifecycle::MATERIALIZED_FORMAT, snapshot])?;
        }
        for observation in note.observations() {
            tx.prepare_cached("INSERT INTO observations(path,category,content,tags,context,line_start,line_end) VALUES(?1,?2,?3,?4,?5,?6,?7)")?.execute(params![item.path,observation.category,observation.content,serde_json::to_string(&observation.tags)?,observation.context,observation.line_range.start as i64,observation.line_range.end as i64])?;
        }
        let mut relations: BTreeSet<_> = note
            .relations()
            .into_iter()
            .map(|r| (r.target, r.rel_type))
            .collect();
        for link in note.wikilinks() {
            // The parser's relation target retains aliases. Add the split identity only
            // if an equivalent explicit relation is absent.
            if !relations
                .iter()
                .any(|(target, _)| target.split('|').next() == Some(link.target.as_str()))
            {
                relations.insert((link.target, "links_to".into()));
            }
        }
        // Extracted text is opaque semantic input. Its sole provenance edge
        // comes from the source record, never from text that resembles a link.
        if matches!(note_type, "extracted_text" | "document")
            && metadata
                .get("parse_semantics")
                .or_else(|| metadata.get(wi_note::LEGACY_PARSE_SEMANTICS_KEY))
                == Some(&Value::Bool(false))
            && let Some(source) = metadata.get("source")
            && source.get("kind").and_then(Value::as_str) == Some("file")
            && source.get("size_bytes").and_then(Value::as_u64).is_some()
            && source
                .get("media_type")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains('/'))
            && source
                .get("entity_external_id")
                .and_then(Value::as_str)
                .is_some_and(|s| {
                    s.len() == 36
                        && s.bytes().enumerate().all(|(i, b)| {
                            if [8, 13, 18, 23].contains(&i) {
                                b == b'-'
                            } else {
                                b.is_ascii_hexdigit()
                            }
                        })
                })
            && let Some(path) = source.get("file_path").and_then(Value::as_str)
            && !path.starts_with('/')
            && !path.contains('\\')
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && item.path == format!("{path}.md")
            && source
                .get("checksum")
                .and_then(Value::as_str)
                .is_some_and(|s| {
                    s.strip_prefix("sha256:").is_some_and(|h| {
                        h.len() == 64
                            && h.bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
                })
        {
            // The target is the original binary, not a note: pin it to that file
            // so identity resolution never maps it onto the sidecar's permalink.
            tx.prepare_cached("INSERT OR IGNORE INTO relations(from_path,to_name,to_path,relation_type,relative_target,target_file) VALUES(?1,?2,?2,'derived_from',NULL,1)")?
                .execute(params![item.path, path])?;
        }
        for (target, kind) in relations {
            let target = target
                .split('|')
                .next()
                .unwrap_or(&target)
                .split('#')
                .next()
                .unwrap_or("");
            tx.prepare_cached(
                "INSERT OR IGNORE INTO relations(from_path,to_name,relation_type,relative_target) VALUES(?1,?2,?3,?4)",
            )?
            .execute(params![item.path, target, kind, relative_target(&item.path, target)])?;
        }
    }
    Ok(())
}
// Persist normalized relative targets so additions/deletions can find backlinks
// without scanning every relation in the vault.
fn relative_target(from: &str, target: &str) -> Option<String> {
    if !target.starts_with("./") && !target.starts_with("../") {
        return None;
    }
    let mut parts: Vec<_> = from.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => (),
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}
fn affected_names(tx: &Transaction<'_>, paths: &[String]) -> Result<BTreeSet<String>> {
    let mut names: BTreeSet<_> = paths.iter().cloned().collect();
    let mut stmt = tx.prepare_cached("SELECT permalink,title FROM notes WHERE path=?1")?;
    for path in paths {
        for row in stmt.query_map([path], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (permalink, title) = row?;
            names.insert(permalink);
            names.insert(title);
        }
    }
    Ok(names)
}
fn resolve_affected(
    tx: &Transaction<'_>,
    paths: &[String],
    names: &BTreeSet<String>,
) -> Result<()> {
    let mut ids = BTreeSet::new();
    for path in paths {
        let mut stmt = tx.prepare_cached("SELECT rowid FROM relations WHERE from_path=?1 AND target_file=0 UNION SELECT rowid FROM relations WHERE relative_target=?1 AND target_file=0")?;
        ids.extend(
            stmt.query_map([path], |r| r.get::<_, i64>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
    }
    let mut stmt =
        tx.prepare_cached("SELECT rowid FROM relations WHERE to_name=?1 AND target_file=0")?;
    for name in names {
        ids.extend(
            stmt.query_map([name], |r| r.get::<_, i64>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
    }
    let mut relation =
        tx.prepare_cached("SELECT to_name,relative_target,to_path FROM relations WHERE rowid=?1")?;
    let mut lookup = tx.prepare_cached("SELECT path FROM notes WHERE path=?1 UNION SELECT path FROM notes WHERE permalink=?1 UNION SELECT path FROM notes WHERE title=?1 UNION SELECT path FROM notes WHERE path=?2")?;
    let mut update = tx.prepare_cached("UPDATE relations SET to_path=?1 WHERE rowid=?2")?;
    for id in ids {
        let (target, relative, old) = relation.query_row([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let candidates = lookup
            .query_map(params![target, relative], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let resolved = if candidates.len() == 1 {
            candidates.into_iter().next()
        } else {
            None
        };
        if resolved != old {
            update.execute(params![resolved, id])?;
        }
    }
    Ok(())
}
fn resolve(tx: &Transaction<'_>) -> Result<()> {
    // Build each identity lookup once. Ambiguous authored identities remain
    // unresolved; adding/removing a target re-resolves existing backlinks.
    let mut identities: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut paths = BTreeSet::new();
    let mut notes = tx.prepare_cached("SELECT path,permalink,title FROM notes")?;
    for row in notes.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (path, permalink, title) = row?;
        paths.insert(path.clone());
        for key in [path.clone(), permalink, title] {
            identities.entry(key).or_default().insert(path.clone());
        }
    }
    let mut relations = tx.prepare_cached(
        "SELECT rowid,from_path,to_name,to_path FROM relations WHERE target_file=0",
    )?;
    let rows = relations
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut update = tx.prepare_cached("UPDATE relations SET to_path=?1 WHERE rowid=?2")?;
    for (id, from, target, old) in rows {
        let mut candidates = identities.get(&target).cloned().unwrap_or_default();
        if let Some(relative) = relative_target(&from, &target)
            && paths.contains(&relative)
        {
            candidates.insert(relative);
        }
        let resolved = if candidates.len() == 1 {
            candidates.into_iter().next()
        } else {
            None
        };
        if old != resolved {
            update.execute(params![resolved, id])?;
        }
    }
    Ok(())
}

/// Case-insensitive literal substring scan, with the same metadata/type predicates.
/// Text is literal, so Boolean/prefix/phrase syntax has no special meaning here.
pub fn literal_search(vault_root: impl AsRef<Path>, query: &Query) -> Result<SearchResult> {
    let predicates = query.validate()?;
    let root = fs::canonicalize(vault_root)?;
    let mut report = SyncReport::default();
    let settings = match settings(&root) {
        Ok(settings) => settings,
        Err(e) => {
            report.diagnostics.push(IndexDiagnostic {
                rel_path: wi_note::VAULT_CONFIG_PATH.into(),
                message: e.to_string(),
            });
            Settings {
                ignored: vec![PathBuf::from(".wiki-island/local")],
                max_note_bytes: DEFAULT_MAX_NOTE_BYTES,
            }
        }
    };
    let files = walk(&root, settings.ignored, &mut report);
    let needle = query.text.trim().to_lowercase();
    // Reuse exactly the indexed JSON predicates, without opening an index file.
    // This keeps SQLite numeric coercion and null behavior identical in fallback.
    let filter_db = Connection::open_in_memory()?;
    let mut filter_params = vec![SqlValue::Null];
    let conditions: Vec<_> = predicates
        .iter()
        .map(|p| p.sql(&mut filter_params))
        .collect();
    let filter_sql = format!(
        "SELECT coalesce(({}),0) FROM (SELECT ?1 AS metadata) n",
        if conditions.is_empty() {
            "1".into()
        } else {
            conditions.join(" AND ")
        }
    );
    let mut filter = filter_db.prepare(&filter_sql)?;
    let mut hits = Vec::new();
    let mut matched = 0usize;
    if query.limit == 0 && query.count_limit == 0 {
        return Ok(SearchResult {
            hits,
            index_as_of: None,
            fallback: true,
            skipped: report.skipped,
            diagnostics: report.diagnostics,
            matches: None,
        });
    }
    for (path, s) in files {
        // Engine input is indexed for the engine, never delivered as a note.
        if wi_note::is_hidden_engine_path(&path) {
            continue;
        }
        let item = match prepare_retry(&root, &path, &s, None, settings.max_note_bytes) {
            Ok(item) => item,
            Err(e) => {
                skip(&mut report, &path, e);
                continue;
            }
        };
        let Some(note) = item.note else {
            continue;
        };
        report
            .diagnostics
            .extend(note.diagnostics().iter().map(|d| IndexDiagnostic {
                rel_path: path.clone().into(),
                message: d.message.clone(),
            }));
        let metadata = metadata(&note);
        if query.type_filter.as_ref().is_some_and(|t| {
            metadata
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("note")
                != t
        }) {
            continue;
        }
        let (permalink, title) = names(&path, &metadata);
        let haystack = match query.mode {
            SearchMode::Text => format!("{title}\n{}", note.body()),
            SearchMode::Title => title.clone(),
            SearchMode::Permalink => permalink.clone(),
        }
        .to_lowercase();
        let found = if needle.is_empty() || needle == "*" {
            true
        } else if query.mode == SearchMode::Permalink && needle.contains('*') {
            query::star_match(&needle, &haystack)
        } else {
            memchr::memmem::find(haystack.as_bytes(), needle.as_bytes()).is_some()
        };
        if found {
            filter_params[0] = SqlValue::Text(metadata.to_string());
            if !filter.query_row(params_from_iter(&filter_params), |r| r.get::<_, bool>(0))? {
                continue;
            }
            matched += 1;
            if hits.len() < query.limit {
                hits.push(make_hit(
                    path,
                    permalink,
                    note.body(),
                    metadata,
                    item.hash,
                    0.0,
                    true,
                    &query.text,
                ));
            }
            if hits.len() == query.limit && matched > query.count_limit {
                break;
            }
        }
    }
    Ok(SearchResult {
        hits,
        index_as_of: None,
        fallback: true,
        skipped: report.skipped,
        diagnostics: report.diagnostics,
        matches: (query.count_limit > 0).then_some(matched),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unsupported_byte_paths_have_serializable_diagnostics() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let mut report = SyncReport::default();
        skip(
            &mut report,
            PathBuf::from(OsString::from_vec(b"bad\xff.md".to_vec())),
            "unsupported path",
        );
        assert_eq!(report.skipped.len(), 1);
        serde_json::to_string(&report).unwrap();
    }

    #[test]
    fn stale_walk_stamp_is_retried_once_and_missing_file_is_reportable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        fs::write(&path, "new content").unwrap();
        let mut stale = stamp(&path).unwrap();
        stale.size += 1;
        let item =
            prepare_retry(dir.path(), "note.md", &stale, None, DEFAULT_MAX_NOTE_BYTES).unwrap();
        assert_eq!(item.hash, content_hash(b"new content"));
        fs::remove_file(path).unwrap();
        let error = prepare_retry(dir.path(), "note.md", &stale, None, DEFAULT_MAX_NOTE_BYTES)
            .err()
            .unwrap();
        let mut report = SyncReport::default();
        skip(&mut report, "note.md", error);
        assert_eq!(report.skipped, [Path::new("note.md")]);
    }

    #[test]
    fn directory_patterns_match_the_directory_itself() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir(dir.path().join("ignored")).unwrap();
        let rel = Path::new("ignored");
        assert!(!ignored(dir.path(), rel), "a file named `ignored` is kept");
        assert!(ignored_dir(dir.path(), rel), "the directory is pruned");
        assert!(ignored(dir.path(), Path::new("ignored/x.md")));
        assert!(!ignored_dir(dir.path(), Path::new("kept")));
    }

    #[cfg(unix)]
    #[test]
    fn walk_skips_an_unreadable_ignored_directory_without_errors() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::write(dir.path().join("kept.md"), "kept").unwrap();
        let hidden = dir.path().join("ignored");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("x.md"), "x").unwrap();
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
        let mut report = SyncReport::default();
        let files = walk(dir.path(), Vec::new(), &mut report);
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(files.keys().collect::<Vec<_>>(), ["kept.md"]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
    }
}
