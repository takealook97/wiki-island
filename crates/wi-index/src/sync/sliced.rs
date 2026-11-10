//! Rebuild in slices: a rebuild a caller can stop between chunks and resume
//! later, even from another process, without the index ever claiming a
//! complete walk before the last slice.
//!
//! Progress is the `rebuild_after` row of `index_meta` (the last path
//! rebuilt, `""` before the first chunk). Each chunk commits its rows and
//! the new progress in one transaction. While the row exists the
//! `index_as_of` row does not, so search answers from the literal fallback.
//! The last slice stat-compares every file with its row (catching edits
//! made to already rebuilt paths), removes rows of files gone, resolves
//! links and records the walk, like [`Index::sync`].

use super::*;
use rusqlite::OptionalExtension;
use std::ops::Bound;

const PROGRESS: &str = "rebuild_after";

fn upsert(tx: &rusqlite::Transaction<'_>, key: &str, value: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO index_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key, value],
    )?;
    Ok(())
}

fn read_meta(tx: &rusqlite::Transaction<'_>, key: &str) -> Result<Option<String>> {
    Ok(tx
        .query_row("SELECT value FROM index_meta WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

/// Forget an unfinished sliced rebuild (a complete walk ends it).
pub(super) fn clear_progress(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute("DELETE FROM index_meta WHERE key=?1", [PROGRESS])?;
    Ok(())
}

impl Index {
    /// The last path an unfinished sliced rebuild stored (`""` before its
    /// first chunk); `None` when none is in progress. A recorded complete
    /// walk always wins: then no rebuild is in progress.
    pub fn rebuild_progress(&self) -> Result<Option<String>> {
        if self.index_as_of()?.is_some() {
            return Ok(None);
        }
        Ok(self
            .connection
            .query_row(
                "SELECT value FROM index_meta WHERE key=?1",
                [PROGRESS],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Start (or restart) a sliced rebuild: withdraw the complete-walk
    /// claim and record that no path is rebuilt yet, in one transaction.
    pub fn begin_rebuild(&mut self) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM index_meta WHERE key='index_as_of'", [])?;
        for key in ["skipped", "diagnostics"] {
            tx.execute(
                "INSERT OR IGNORE INTO index_meta(key,value) VALUES(?1,'[]')",
                [key],
            )?;
        }
        upsert(&tx, PROGRESS, "")?;
        tx.commit()?;
        Ok(())
    }

    /// Run one slice of a sliced rebuild, starting one when none is in
    /// progress: rebuild `chunk` paths at a time until `over()` says stop
    /// (checked after every chunk, so at least one runs). Returns the walk
    /// report when this slice finished the rebuild; its counts cover this
    /// slice only.
    pub fn rebuild_slice(
        &mut self,
        chunk: usize,
        over: &dyn Fn() -> bool,
    ) -> Result<Option<SyncReport>> {
        let start = Instant::now();
        let mut after = match self.rebuild_progress()? {
            Some(after) => after,
            None => {
                self.begin_rebuild()?;
                String::new()
            }
        };
        let as_of = now();
        let settings = settings(&self.vault_root)?;
        let mut walked = SyncReport::default();
        let files = walk(&self.vault_root, settings.ignored.clone(), &mut walked);
        let mut report = SyncReport::default();
        loop {
            let batch: Vec<(&String, &Stamp)> = files
                .range::<str, _>((Bound::Excluded(after.as_str()), Bound::Unbounded))
                .take(chunk.max(1))
                .collect();
            let Some((last, _)) = batch.last() else {
                break;
            };
            let last = (*last).clone();
            self.rebuild_chunk(&batch, &last, settings.max_note_bytes, &mut report)?;
            after = last;
            if over() {
                return Ok(None);
            }
        }
        let mut report = self.finish_rebuild(&as_of, &files, walked, report, &settings)?;
        report.elapsed_ms = start.elapsed().as_millis();
        Ok(Some(report))
    }

    /// Reparse `batch` and store it with the new progress in one
    /// transaction. Failed files keep their previous rows, as in a rebuild.
    fn rebuild_chunk(
        &mut self,
        batch: &[(&String, &Stamp)],
        last: &str,
        max_bytes: u64,
        report: &mut SyncReport,
    ) -> Result<()> {
        let mut chunk = SyncReport::default();
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let size = batch_size(batch.iter().map(|(_, stamp)| stamp.size));
        for part in batch.chunks(size) {
            let prepared: Vec<_> = part
                .par_iter()
                .map(|(path, stamp)| prepare_retry(&self.vault_root, path, stamp, None, max_bytes))
                .collect();
            for ((path, _), item) in part.iter().zip(prepared) {
                match item {
                    Ok(item) => store_counted(&tx, item, &mut chunk)?,
                    Err(e) => skip(&mut chunk, PathBuf::from(path.as_str()), e),
                }
            }
        }
        let affected: Vec<String> = batch.iter().map(|(path, _)| (*path).clone()).collect();
        refresh_walk_meta(&tx, &affected, &chunk.skipped, &chunk.diagnostics)?;
        upsert(&tx, PROGRESS, last)?;
        tx.commit()?;
        report.hashed += chunk.hashed;
        report.parsed += chunk.parsed;
        report.updated += chunk.updated;
        report.added += chunk.added;
        report.skipped.extend(chunk.skipped);
        report.diagnostics.extend(chunk.diagnostics);
        Ok(())
    }

    /// The last step: bring rows rebuilt by earlier slices up to this walk,
    /// resolve links and record the complete walk.
    fn finish_rebuild(
        &mut self,
        as_of: &str,
        files: &BTreeMap<String, Stamp>,
        walked: SyncReport,
        mut report: SyncReport,
        settings: &Settings,
    ) -> Result<SyncReport> {
        let existing = self.existing()?;
        let unknown_walk_error = walked.skipped.iter().any(|p| p.as_os_str().is_empty());
        let walk_skipped: BTreeSet<_> = walked.skipped.iter().collect();
        let removed: Vec<_> = existing
            .keys()
            .filter(|p| {
                !files.contains_key(*p)
                    && !unknown_walk_error
                    && !walk_skipped.contains(&PathBuf::from(p.as_str()))
            })
            .cloned()
            .collect();
        let changed: Vec<_> = files
            .iter()
            .filter(|(p, s)| {
                s.size > settings.max_note_bytes || existing.get(*p).is_none_or(|e| &e.stamp != *s)
            })
            .collect();
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for path in &removed {
            remove(&tx, path)?;
        }
        let mut fixed = SyncReport::default();
        let size = batch_size(changed.iter().map(|(_, stamp)| stamp.size));
        for part in changed.chunks(size) {
            let prepared: Vec<_> = part
                .par_iter()
                .map(|(path, stamp)| {
                    prepare_retry(
                        &self.vault_root,
                        path,
                        stamp,
                        existing.get(*path),
                        settings.max_note_bytes,
                    )
                })
                .collect();
            for ((path, _), item) in part.iter().zip(prepared) {
                match item {
                    Ok(item) => store_counted(&tx, item, &mut fixed)?,
                    Err(e) => skip(&mut fixed, PathBuf::from(path.as_str()), e),
                }
            }
        }
        resolve(&tx)?;
        let touched: BTreeSet<&str> = changed.iter().map(|(p, _)| p.as_str()).collect();
        let keep = |p: &Path| {
            let key = path_key(p);
            files.contains_key(&key) && !touched.contains(key.as_str())
        };
        let stored_skipped: Vec<PathBuf> = match read_meta(&tx, "skipped")? {
            Some(text) => serde_json::from_str(&text)?,
            None => Vec::new(),
        };
        let stored_diagnostics: Vec<IndexDiagnostic> = match read_meta(&tx, "diagnostics")? {
            Some(text) => serde_json::from_str(&text)?,
            None => Vec::new(),
        };
        let skipped: Vec<PathBuf> = walked
            .skipped
            .iter()
            .cloned()
            .chain(stored_skipped.into_iter().filter(|p| keep(p)))
            .chain(fixed.skipped.iter().cloned())
            .collect();
        let diagnostics: Vec<IndexDiagnostic> = walked
            .diagnostics
            .iter()
            .cloned()
            .chain(stored_diagnostics.into_iter().filter(|d| keep(&d.rel_path)))
            .chain(fixed.diagnostics.iter().cloned())
            .collect();
        upsert(&tx, "skipped", &serde_json::to_string(&skipped)?)?;
        upsert(&tx, "diagnostics", &serde_json::to_string(&diagnostics)?)?;
        upsert(&tx, "index_as_of", as_of)?;
        clear_progress(&tx)?;
        tx.commit()?;
        report.hashed += fixed.hashed;
        report.parsed += fixed.parsed;
        report.updated += fixed.updated;
        report.added += fixed.added;
        Ok(SyncReport {
            discovered: files.len(),
            removed: removed.len(),
            unchanged: files.len().saturating_sub(changed.len()),
            index_as_of: Some(as_of.to_owned()),
            skipped,
            diagnostics,
            ..report
        })
    }
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
