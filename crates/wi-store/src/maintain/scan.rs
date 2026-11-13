use std::collections::{BTreeMap, BTreeSet};

use super::{Cursor, Scan};
use crate::contained::read_rel;
use crate::error::Result;
use crate::vault::Vault;
use wi_note::Note;

/// Notes the scan reads before it checks the clock again.
const SCAN_CHUNK: usize = 64;

impl Vault {
    /// Read the notes the detectors compare, [`SCAN_CHUNK`] at a time;
    /// false when the budget stopped the scan (`cursor.offset` records how
    /// far it got). A scan this handle did not start, or one older than
    /// the cursor, starts over; a resumed one first re-reads what changed.
    pub(super) fn maintain_read_notes(
        &self,
        cursor: &mut Cursor,
        over: &dyn Fn() -> bool,
    ) -> Result<bool> {
        let mut slot = self.maintain_scan.borrow_mut();
        let resumable = cursor.offset > 0
            && slot.as_ref().is_some_and(|s| {
                s.next == cursor.offset && Some(s.mark.generation()) == cursor.generation
            });
        if cursor.offset > 0 && !resumable {
            cursor.lost_scans += 1;
        }
        let whole = cursor.lost_scans >= 2;
        let scan = match slot.as_mut() {
            Some(scan) if resumable => {
                self.refresh_scan(scan)?;
                scan
            }
            _ => slot.insert(self.start_scan()?),
        };
        let work = self.work_paths()?;
        loop {
            cursor.offset = scan.next;
            cursor.generation = Some(scan.mark.generation());
            if scan.next >= scan.paths.len() {
                return Ok(true);
            }
            let end = (scan.next + SCAN_CHUNK).min(scan.paths.len());
            self.read_scan_paths(&work, scan, end)?;
            if scan.next < scan.paths.len() && !whole && over() {
                cursor.offset = scan.next;
                return Ok(false);
            }
        }
    }

    /// A new scan: the mark first, so every later publication is seen as
    /// a change.
    fn start_scan(&self) -> Result<Scan> {
        let mark = self.publication_mark()?;
        Ok(Scan {
            paths: self.detection_paths()?,
            next: 0,
            notes: BTreeMap::new(),
            mark,
        })
    }

    /// Read `scan.paths[scan.next..end]`.
    fn read_scan_paths(&self, work: &BTreeSet<String>, scan: &mut Scan, end: usize) -> Result<()> {
        for path in &scan.paths[scan.next..end] {
            match self.detection_note(work, path)? {
                Some(note) => {
                    scan.notes.insert(path.clone(), note);
                }
                None => {
                    scan.notes.remove(path);
                }
            }
        }
        scan.next = end;
        Ok(())
    }

    /// Bring `scan` up to the publications since its mark: forget what it
    /// read of a changed path and queue the path again; start over when
    /// the changed paths are unknown.
    fn refresh_scan(&self, scan: &mut Scan) -> Result<()> {
        let mark = self.publication_mark()?;
        if mark == scan.mark {
            return Ok(());
        }
        let Some(changed) = self.changed_since(&scan.mark)? else {
            *scan = self.start_scan()?;
            return Ok(());
        };
        for path in changed {
            if !crate::paths::is_markdown(&path) {
                continue;
            }
            scan.notes.remove(&path);
            if !scan.paths[scan.next..].contains(&path) {
                scan.paths.push(path);
            }
        }
        scan.mark = mark;
        Ok(())
    }

    /// The completed scan's notes and the mark they reflect, revalidated
    /// against the publications since the scan read them; a `budgeted`
    /// pass re-reads at most one chunk of changed paths. `None` when there
    /// is no scan on this handle or more than that changed.
    pub(super) fn scan_input(
        &self,
        budgeted: bool,
    ) -> Result<
        Option<(
            Vec<wi_lifecycle::detect::DetectionNote>,
            wi_history::PublicationMark,
        )>,
    > {
        let mut slot = self.maintain_scan.borrow_mut();
        let Some(scan) = slot.as_mut() else {
            return Ok(None);
        };
        self.refresh_scan(scan)?;
        if budgeted && scan.paths.len() - scan.next > SCAN_CHUNK {
            return Ok(None);
        }
        let end = scan.paths.len();
        if scan.next < end {
            let work = self.work_paths()?;
            self.read_scan_paths(&work, scan, end)?;
        }
        let Some(scan) = slot.take() else {
            return Ok(None);
        };
        Ok(Some((scan.notes.into_values().collect(), scan.mark)))
    }

    /// Markdown paths the detectors look at: engine-generated exports
    /// repeat rules by design and import records only list pages, so
    /// neither is compared.
    fn detection_paths(&self) -> Result<Vec<String>> {
        let paths: BTreeSet<String> = self
            .with_index(|index| Ok(index.name_rows()?))?
            .map(|rows| rows.into_iter().map(|row| row.path).collect())
            .unwrap_or_else(|| crate::ingest::walk_files(self.root()).into_iter().collect());
        let config = self.config();
        Ok(paths
            .into_iter()
            .filter(|path| {
                crate::paths::is_markdown(path)
                    && !crate::paths::is_generated_export(config, path)
                    && !crate::paths::is_under(path, crate::paths::IMPORTS_DIR)
            })
            .collect())
    }

    /// Paths of the lifecycle snapshot's work notes. A scan keeps only
    /// these while it reads, not the whole snapshot.
    fn work_paths(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .lifecycle_snapshot(&[])?
            .notes()
            .iter()
            .filter(|n| n.lifecycle.lifecycle_type == wi_note::LifecycleType::Work)
            .map(|n| n.path.clone())
            .collect())
    }

    /// Detector input for one path; none for work notes (`work`) and
    /// unreadable or vanished files.
    fn detection_note(
        &self,
        work: &BTreeSet<String>,
        path: &str,
    ) -> Result<Option<wi_lifecycle::detect::DetectionNote>> {
        if work.contains(path) {
            return Ok(None);
        }
        // An unreadable file has its own ingest-failed item; it never stops the pass.
        Ok(read_rel(self.root(), path)
            .ok()
            .flatten()
            .and_then(|bytes| Note::parse(&bytes).ok())
            .map(|parsed| wi_lifecycle::detect::DetectionNote::from_note(path, &parsed)))
    }
}
