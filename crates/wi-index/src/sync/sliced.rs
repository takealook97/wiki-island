//! Rebuild in slices: a rebuild a caller can stop between chunks and resume
//! later, even from another process, without the index ever claiming a
//! complete walk before the last slice.
//!
//! State lives in `index_meta`: `rebuild_id` names the rebuild,
//! `rebuild_after` is the last path rebuilt (`""` before the first chunk)
//! and `rebuild_finish_after` the last path the last step reparsed. While a
//! rebuild runs the `index_as_of` row is absent, so search answers from the
//! literal fallback. Every chunk is one transaction that first checks the
//! rebuild it belongs to is still the active one (same id, no complete
//! walk) and otherwise writes nothing, so a rebuild another connection
//! completed or restarted is never touched again.
//!
//! When no path is left after the cursor, the last step reparses, in
//! chunks too, every file whose stat differs from its row (edits to
//! already rebuilt paths, files added before the cursor). It then removes
//! rows of files gone, resolves links and records the walk in one
//! transaction, like [`Index::sync`].

use super::*;
use rusqlite::OptionalExtension;
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering};

const PROGRESS: &str = "rebuild_after";
const FINISH: &str = "rebuild_finish_after";
const ID: &str = "rebuild_id";

fn upsert(tx: &rusqlite::Transaction<'_>, key: &str, value: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO index_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key, value],
    )?;
    Ok(())
}

fn read_meta(conn: &rusqlite::Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM index_meta WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

/// End an unfinished sliced rebuild (a complete walk ends it). True when
/// one was in progress: the walk must then do its deferred link resolution.
pub(super) fn end_rebuild(tx: &rusqlite::Transaction<'_>) -> Result<bool> {
    let active = read_meta(tx, PROGRESS)?.is_some();
    for key in [PROGRESS, FINISH, ID] {
        tx.execute("DELETE FROM index_meta WHERE key=?1", [key])?;
    }
    Ok(active)
}

/// The cursors of rebuild `id` when it is still the active rebuild.
struct Cursors {
    after: String,
    finish: Option<String>,
}

fn active(conn: &rusqlite::Connection, id: Option<&str>) -> Result<Option<(String, Cursors)>> {
    if read_meta(conn, "index_as_of")?.is_some() {
        return Ok(None);
    }
    let (Some(stored), Some(after)) = (read_meta(conn, ID)?, read_meta(conn, PROGRESS)?) else {
        return Ok(None);
    };
    if id.is_some_and(|id| id != stored) {
        return Ok(None);
    }
    let finish = read_meta(conn, FINISH)?;
    Ok(Some((stored, Cursors { after, finish })))
}

fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        now()
    )
}

/// What one transaction of a slice did.
enum Step {
    /// A chunk was stored (or the last step restarted its pass).
    Stored,
    /// The rebuild is no longer the active one; nothing was written.
    Stopped,
    /// The walk was recorded.
    Finished(SyncReport),
}

/// One slice's fixed inputs.
struct Slice<'a> {
    id: &'a str,
    as_of: &'a str,
    files: &'a BTreeMap<String, Stamp>,
    walked: &'a SyncReport,
    settings: &'a Settings,
    chunk: usize,
}

/// Whether the row of `path` must be read again to match the walk.
fn stale(path: &str, stamp: &Stamp, existing: &BTreeMap<String, Existing>, max: u64) -> bool {
    stamp.size > max || existing.get(path).is_none_or(|e| &e.stamp != stamp)
}

impl Index {
    /// The last path an unfinished sliced rebuild stored (`""` before its
    /// first chunk); `None` when none is in progress. A cursor left next
    /// to a recorded walk is reported too: the next slice restarts then.
    pub fn rebuild_progress(&self) -> Result<Option<String>> {
        read_meta(&self.connection, PROGRESS)
    }

    /// Start (or restart) a sliced rebuild: withdraw the complete-walk
    /// claim and record a new rebuild with no path rebuilt yet, in one
    /// transaction. Chunks of an earlier rebuild stop writing.
    pub fn begin_rebuild(&mut self) -> Result<()> {
        self.start_rebuild().map(drop)
    }

    fn start_rebuild(&mut self) -> Result<String> {
        let id = new_id();
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM index_meta WHERE key IN ('index_as_of',?1)",
            [FINISH],
        )?;
        for key in ["skipped", "diagnostics"] {
            tx.execute(
                "INSERT OR IGNORE INTO index_meta(key,value) VALUES(?1,'[]')",
                [key],
            )?;
        }
        upsert(&tx, PROGRESS, "")?;
        upsert(&tx, ID, &id)?;
        tx.commit()?;
        Ok(id)
    }

    /// Run one slice of a sliced rebuild, starting one when none is in
    /// progress: store `chunk` files per transaction until `over()` says
    /// stop (checked after every chunk, so at least one runs). Returns the
    /// walk report when this slice finished the rebuild; its counts cover
    /// this slice only. Returns `None` without writing when another
    /// connection completed or restarted the rebuild meanwhile.
    pub fn rebuild_slice(
        &mut self,
        chunk: usize,
        over: &dyn Fn() -> bool,
    ) -> Result<Option<SyncReport>> {
        let start = Instant::now();
        let id = match active(&self.connection, None)? {
            Some((id, _)) => id,
            None => self.start_rebuild()?,
        };
        let as_of = now();
        let settings = settings(&self.vault_root)?;
        let mut walked = SyncReport::default();
        let files = walk(&self.vault_root, settings.ignored.clone(), &mut walked);
        let slice = Slice {
            id: &id,
            as_of: &as_of,
            files: &files,
            walked: &walked,
            settings: &settings,
            chunk: chunk.max(1),
        };
        let mut report = SyncReport::default();
        let mut touched = BTreeSet::new();
        loop {
            match self.slice_step(&slice, &mut touched, &mut report)? {
                Step::Stored if over() => return Ok(None),
                Step::Stored => {}
                Step::Stopped => return Ok(None),
                Step::Finished(mut done) => {
                    done.elapsed_ms = start.elapsed().as_millis();
                    return Ok(Some(done));
                }
            }
        }
    }

    /// One transaction of a slice: the next rebuild chunk, the next chunk
    /// of the last step, or the final record.
    fn slice_step(
        &mut self,
        slice: &Slice<'_>,
        touched: &mut BTreeSet<String>,
        report: &mut SyncReport,
    ) -> Result<Step> {
        let root = self.vault_root.clone();
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some((_, cursors)) = active(&tx, Some(slice.id))? else {
            return Ok(Step::Stopped);
        };
        let max = slice.settings.max_note_bytes;
        if cursors.finish.is_none() {
            let batch: Vec<(&String, &Stamp)> = slice
                .files
                .range::<str, _>((Bound::Excluded(cursors.after.as_str()), Bound::Unbounded))
                .take(slice.chunk)
                .collect();
            if let Some((last, _)) = batch.last() {
                let last = (*last).clone();
                store_chunk(&tx, &root, &batch, None, max, touched, report)?;
                upsert(&tx, PROGRESS, &last)?;
                tx.commit()?;
                return Ok(Step::Stored);
            }
        }
        let finish = cursors.finish.unwrap_or_default();
        let existing = existing_rows(&tx)?;
        let batch: Vec<(&String, &Stamp)> = slice
            .files
            .range::<str, _>((Bound::Excluded(finish.as_str()), Bound::Unbounded))
            .filter(|(p, s)| !touched.contains(*p) && stale(p, s, &existing, max))
            .take(slice.chunk)
            .collect();
        if let Some((last, _)) = batch.last() {
            let last = (*last).clone();
            store_chunk(&tx, &root, &batch, Some(&existing), max, touched, report)?;
            upsert(&tx, FINISH, &last)?;
            tx.commit()?;
            return Ok(Step::Stored);
        }
        // Rows this pass already went past may have changed again since;
        // files whose read failed in this rebuild are not retried.
        let skipped: BTreeSet<String> = stored_skipped(&tx)?.iter().map(|p| path_key(p)).collect();
        let again = slice.files.iter().any(|(p, s)| {
            !touched.contains(p) && !skipped.contains(p) && stale(p, s, &existing, max)
        });
        if again {
            upsert(&tx, FINISH, "")?;
            tx.commit()?;
            return Ok(Step::Stored);
        }
        let done = finish_rebuild(&tx, slice, &existing, touched, std::mem::take(report))?;
        tx.commit()?;
        Ok(Step::Finished(done))
    }
}

/// Read `batch` (reusing parsed rows whose hash is unchanged when
/// `existing` is given) and store it. Failed files keep their previous
/// rows, as in a rebuild.
fn store_chunk(
    tx: &rusqlite::Transaction<'_>,
    root: &Path,
    batch: &[(&String, &Stamp)],
    existing: Option<&BTreeMap<String, Existing>>,
    max_bytes: u64,
    touched: &mut BTreeSet<String>,
    report: &mut SyncReport,
) -> Result<()> {
    let mut chunk = SyncReport::default();
    let size = batch_size(batch.iter().map(|(_, stamp)| stamp.size));
    for part in batch.chunks(size) {
        let prepared: Vec<_> = part
            .par_iter()
            .map(|(path, stamp)| {
                let old = existing.and_then(|e| e.get(path.as_str()));
                prepare_retry(root, path, stamp, old, max_bytes)
            })
            .collect();
        for ((path, _), item) in part.iter().zip(prepared) {
            match item {
                Ok(item) => store_counted(tx, item, &mut chunk)?,
                Err(e) => skip(&mut chunk, PathBuf::from(path.as_str()), e),
            }
        }
    }
    let affected: Vec<String> = batch.iter().map(|(path, _)| (*path).clone()).collect();
    refresh_walk_meta(tx, &affected, &chunk.skipped, &chunk.diagnostics)?;
    touched.extend(affected);
    report.hashed += chunk.hashed;
    report.parsed += chunk.parsed;
    report.updated += chunk.updated;
    report.added += chunk.added;
    report.skipped.extend(chunk.skipped);
    report.diagnostics.extend(chunk.diagnostics);
    Ok(())
}

fn stored_skipped(conn: &rusqlite::Connection) -> Result<Vec<PathBuf>> {
    Ok(match read_meta(conn, "skipped")? {
        Some(text) => serde_json::from_str(&text)?,
        None => Vec::new(),
    })
}

/// The final record, once every row matches the walk: remove rows of
/// files gone, resolve links, keep walk metadata of existing files and
/// record the complete walk.
fn finish_rebuild(
    tx: &rusqlite::Transaction<'_>,
    slice: &Slice<'_>,
    existing: &BTreeMap<String, Existing>,
    touched: &BTreeSet<String>,
    report: SyncReport,
) -> Result<SyncReport> {
    let (files, walked) = (slice.files, slice.walked);
    let unknown_walk_error = walked.skipped.iter().any(|p| p.as_os_str().is_empty());
    let walk_skipped: BTreeSet<_> = walked.skipped.iter().collect();
    let removed: Vec<_> = existing
        .keys()
        .filter(|p| {
            !files.contains_key(*p)
                && !unknown_walk_error
                && !walk_skipped.contains(&PathBuf::from(p.as_str()))
        })
        .collect();
    for path in &removed {
        remove(tx, path)?;
    }
    resolve(tx)?;
    let keep = |p: &Path| files.contains_key(&path_key(p));
    let stored_diagnostics: Vec<IndexDiagnostic> = match read_meta(tx, "diagnostics")? {
        Some(text) => serde_json::from_str(&text)?,
        None => Vec::new(),
    };
    let skipped: Vec<PathBuf> = walked
        .skipped
        .iter()
        .cloned()
        .chain(stored_skipped(tx)?.into_iter().filter(|p| keep(p)))
        .collect();
    let diagnostics: Vec<IndexDiagnostic> = walked
        .diagnostics
        .iter()
        .cloned()
        .chain(stored_diagnostics.into_iter().filter(|d| keep(&d.rel_path)))
        .collect();
    upsert(tx, "skipped", &serde_json::to_string(&skipped)?)?;
    upsert(tx, "diagnostics", &serde_json::to_string(&diagnostics)?)?;
    upsert(tx, "index_as_of", slice.as_of)?;
    end_rebuild(tx)?;
    Ok(SyncReport {
        discovered: files.len(),
        removed: removed.len(),
        unchanged: files.len().saturating_sub(touched.len()),
        index_as_of: Some(slice.as_of.to_owned()),
        skipped,
        diagnostics,
        ..report
    })
}

/// Store a prepared file and count it in `report`.
fn store_counted(
    tx: &rusqlite::Transaction<'_>,
    item: Prepared,
    report: &mut SyncReport,
) -> Result<()> {
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
    let known: bool = tx
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM files WHERE path=?1)")?
        .query_row([&item.path], |r| r.get(0))?;
    if known {
        report.updated += 1;
    } else {
        report.added += 1;
    }
    store(tx, item)
}
