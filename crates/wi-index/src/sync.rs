use crate::{Error, Index, Result, content_hash, now};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    time::{Instant, UNIX_EPOCH},
};
use wi_note::Note;

mod literal;
mod sliced;
mod storage;
mod walk;

pub use literal::literal_search;
use storage::*;
use walk::*;
pub use walk::{
    exclude_matcher, ignored, ignored_dir, ignored_with_errors, set_exclude_file, walk_excluded,
};

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
        || (!cfg!(windows) && path.to_string_lossy().contains('\\'))
    {
        return Err(Error::InvalidPath(path.into()));
    }
    Ok(())
}
fn path_key(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn stamp(path: &Path) -> Result<Stamp> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || is_link(&metadata) {
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
        if is_link(&fs::symlink_metadata(&ancestor)?) {
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
        sliced::clear_progress(&tx)?;
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
            let text = path_key(path);
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
        let affected: Vec<_> = paths.iter().map(|p| path_key(p.as_ref())).collect();
        let identities = affected_names(&tx, &affected)?;
        let mut count = 0;
        for path in &affected {
            count += remove(&tx, path)?;
        }
        resolve_affected(&tx, &affected, &identities)?;
        refresh_walk_meta(&tx, &affected, &[], &[])?;
        tx.commit()?;
        Ok(count)
    }
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
