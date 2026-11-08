//! Self-maintenance: one idempotent, deterministic, model-free pass that
//! keeps a vault healthy without anyone asking (`wi maintain`, and the
//! watcher on a schedule).
//!
//! In order, each stage silent when there is nothing to do: retry held
//! recovery intents; repair a pending index; check the index (rebuild only
//! when it is unusable or has no complete walk; a full pass also runs the
//! stat-walk catch-up); move work notes out of the folder older vaults
//! used; materialize the built-in maintenance procedure; the lifecycle
//! sweep (resume the queue, complete supersedes, flag conflicts, convert
//! work requests, create or refresh work items); read the notes the
//! detectors need, in chunks; procedure health and detector items, and
//! closing items whose cause is gone; procedure change detection; flush.
//! A pass that finds nothing writes nothing and commits nothing.
//!
//! In folder mode only the primary device (`primary_device` in the vault
//! config) runs the stages that write shared notes (migration, the
//! built-in procedure and every lifecycle stage); other devices run
//! recovery, the index and procedure change detection.
//!
//! With a time budget a pass stops between stages or between chunks of
//! the work-note migration or the note scan, reports `complete: false`
//! and records where it stopped in the per-device local directory (device-local,
//! never committed). The next budgeted pass resumes there, so a slice
//! exceeds its budget by at most one stage or chunk. A pass without a
//! budget always runs every stage.
//!
//! The scan remembers the publication mark it read at. When the vault
//! changes between slices, the next slice re-reads only the paths
//! published since that mark (or starts over when they are unknown), and
//! the health stage revalidates the scan once more before it uses it, so
//! a completed pass never judges stale notes. A handle that did not read
//! the scan itself (reopened mid-pass) reads it again in chunks.
//!
//! Every engine publication of one pass belongs to one batch, so a pass
//! that changes several notes makes one commit.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wi_lifecycle::{Context, Outcome, Snapshot, State, WorkItem};
use wi_note::Note;

use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::ops::MoveRequest;
use crate::vault::{Ingress, Vault};

/// What one maintenance pass may do.
#[derive(Debug, Clone, Default)]
pub struct MaintainOptions {
    /// Also run the checks that read every file (stat-walk catch-up).
    pub full: bool,
    /// Stop between steps once this much time has passed.
    pub time_budget: Option<Duration>,
}

/// What one maintenance pass did. All zero / false when nothing changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MaintainReport {
    /// Held intents settled plus index paths repaired.
    pub repaired: usize,
    /// The device index was rebuilt from the files.
    pub index_rebuilt: bool,
    /// Work notes created (lifecycle sweep and procedure health).
    pub work_created: usize,
    /// Work notes closed because their cause is gone.
    pub work_closed: usize,
    /// A procedure note was added, changed or removed since the last pass
    /// on this device (host skills need a sync).
    pub procedures_changed: bool,
    /// Notes moved out of the folders older vaults used.
    pub migrated: usize,
    pub elapsed_ms: u64,
    /// False when the time budget stopped the pass early.
    pub complete: bool,
}

/// Per-device memory of the last pass (`.git/wi/maintain-state.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct DeviceState {
    /// Digest of the procedure notes the last pass saw.
    procedures: Option<String>,
}

const STATE_FILE: &str = "maintain-state.json";

/// Reason prefix of the conflict item a failed adoption opens.
const CANDIDATE_CONFLICT: &str = "candidate conflicts with an active claim";

/// `related` prefix of an item converted from a `missing` work request.
const MISSING_PREFIX: &str = "missing: ";

/// `related` prefix of an item converted from an `ingest-failed` request.
const INGEST_FAILED_PREFIX: &str = "ingest-failed: ";

/// Where a budget-stopped pass resumes (device-local).
/// Inside the per-device local directory ([`Vault::local_dir`]).
const CURSOR_FILE: &str = "maintain-cursor.json";

/// Notes the scan reads before it checks the clock again.
const SCAN_CHUNK: usize = 64;

/// Files the work-note migration inspects before it checks the clock again.
const MIGRATE_CHUNK: usize = 64;

/// The stages of a pass, in order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Stage {
    #[default]
    Recovery,
    Index,
    Migrate,
    Builtin,
    Deferred,
    Supersedes,
    Conflicts,
    Requests,
    Sweep,
    Scan,
    Health,
    Procedures,
}

const STAGES: [Stage; 12] = [
    Stage::Recovery,
    Stage::Index,
    Stage::Migrate,
    Stage::Builtin,
    Stage::Deferred,
    Stage::Supersedes,
    Stage::Conflicts,
    Stage::Requests,
    Stage::Sweep,
    Stage::Scan,
    Stage::Health,
    Stage::Procedures,
];

/// Persisted progress of a budget-stopped pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    stage: Stage,
    /// Notes the scan already read (only for [`Stage::Scan`]).
    offset: usize,
    /// The pass was asked for the full checks.
    full: bool,
    /// Vault generation the scan's notes reflect (only for [`Stage::Scan`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generation: Option<u64>,
    /// Last file the migration inspected (only for [`Stage::Migrate`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<String>,
    /// Scans of this pass lost because their handle closed (reopened
    /// mid-pass). The first loss rescans in chunks; after a second one the
    /// next handle reads the rest in one slice, so a pass whose every slice
    /// runs on a new handle still finishes.
    #[serde(default, skip_serializing_if = "is_zero")]
    lost_scans: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Detector input read so far by the scan stage of this handle.
#[derive(Debug)]
pub(crate) struct Scan {
    /// Paths to read; `paths[next..]` are still unread.
    paths: Vec<String>,
    next: usize,
    /// Notes read so far, by path.
    notes: BTreeMap<String, wi_lifecycle::detect::DetectionNote>,
    /// Publications up to this mark are reflected in `notes`.
    mark: wi_history::PublicationMark,
}

impl Vault {
    /// Run one maintenance pass (see the module docs).
    pub fn maintain(&self, opts: &MaintainOptions) -> Result<MaintainReport> {
        let start = Instant::now();
        let mut report = MaintainReport::default();
        let over = || opts.time_budget.is_some_and(|b| start.elapsed() >= b);
        let generation = self.generation()?;
        let saved = if opts.time_budget.is_some() {
            self.load_cursor()
        } else {
            None
        };
        let mut cursor = saved.clone().unwrap_or_default();
        if opts.full && !cursor.full {
            cursor = Cursor {
                full: true,
                ..Cursor::default()
            };
        }
        self.maintain_basis.set(None);
        // Judged before the batch closes: its closing record is no change.
        let mut stale = None;
        let outcome = self
            .publication_batch(crate::import::MAINTAIN, || {
                let mut input = None;
                let complete =
                    self.run_stages(&mut cursor, &mut report, opts, &over, &mut input)?;
                if let Some(mark) = input.filter(|_| complete)
                    && self.input_stale(&mark)?
                {
                    stale = Some(mark.generation());
                }
                Ok(complete)
            })
            .and_then(|r| r);
        let complete = match outcome {
            Ok(complete) => complete,
            Err(e) => {
                // The next pass starts over.
                self.maintain_scan.borrow_mut().take();
                self.save_cursor(None, saved.is_some());
                return Err(e);
            }
        };
        if complete {
            self.maintain_scan.borrow_mut().take();
            self.save_cursor(None, saved.is_some());
            let basis = match stale {
                Some(generation) => generation,
                None => self.generation()?,
            };
            self.maintain_basis.set(Some(basis));
        } else if saved.as_ref() != Some(&cursor) {
            self.save_cursor(Some(&cursor), saved.is_some());
        }
        if self.generation()? != generation || self.manifest_pending() {
            self.flush()?;
        }
        report.complete = complete;
        report.elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(report)
    }

    /// Whether a note the detectors read changed after the pass read its
    /// input at `mark`. The pass's own work-note writes do not count.
    fn input_stale(&self, mark: &wi_history::PublicationMark) -> Result<bool> {
        let work = format!("{}/", self.config.work_dir().trim_matches('/'));
        Ok(match self.changed_since(mark)? {
            Some(paths) => paths
                .iter()
                .any(|p| crate::paths::is_markdown(p) && !p.starts_with(&work)),
            None => true,
        })
    }

    /// For the watcher: the generation a complete pass on this handle
    /// leaves behind, or, when a note changed under its detector input,
    /// the older generation that input reflects (so the next check runs a
    /// corrective pass). `None` before a complete pass.
    pub(crate) fn maintain_basis(&self) -> Option<u64> {
        self.maintain_basis.get()
    }

    /// Run the stages from `cursor`; false when the budget stopped them.
    /// At least one stage or chunk runs per call. `input` receives the
    /// mark the detector input reflects.
    fn run_stages(
        &self,
        cursor: &mut Cursor,
        report: &mut MaintainReport,
        opts: &MaintainOptions,
        over: &dyn Fn() -> bool,
        input: &mut Option<wi_history::PublicationMark>,
    ) -> Result<bool> {
        let mut ran = false;
        // Folder mode (S4): only the primary device writes shared notes in
        // maintenance; others keep recovery, the index and change detection.
        let primary = self.is_primary_device();
        let lifecycle = primary && self.lifecycle_on();
        let mut at = STAGES
            .iter()
            .position(|s| *s == cursor.stage)
            .unwrap_or_default();
        while let Some(&stage) = STAGES.get(at) {
            // After a second lost scan, scan and health run in one slice.
            let together = cursor.lost_scans >= 2 && matches!(stage, Stage::Scan | Stage::Health);
            if ran && !together && over() {
                cursor.stage = stage;
                return Ok(false);
            }
            ran = true;
            if stage != cursor.stage {
                cursor.stage = stage;
                cursor.offset = 0;
                cursor.generation = None;
                cursor.after = None;
            }
            match stage {
                Stage::Recovery => self.maintain_recovery(report)?,
                Stage::Index => self.maintain_index(report, opts.full || cursor.full)?,
                Stage::Migrate if primary => {
                    if !self.maintain_migrate(report, cursor, over)? {
                        return Ok(false);
                    }
                }
                Stage::Builtin if primary => self.maintain_builtin(report)?,
                Stage::Deferred if lifecycle => {
                    self.resume_deferred()?;
                }
                Stage::Supersedes if lifecycle => {
                    self.recover_supersedes()?;
                }
                Stage::Conflicts if lifecycle => {
                    self.propagate_conflicts()?;
                }
                Stage::Requests if lifecycle => {
                    report.work_created += self.convert_work_requests()?.len();
                }
                Stage::Sweep if lifecycle => {
                    let mut summary = crate::lifecycle::SweepSummary::default();
                    self.sweep_items(&mut summary)?;
                    report.work_created += summary.work_created.len();
                }
                Stage::Scan if lifecycle => {
                    if !self.maintain_read_notes(cursor, over)? {
                        return Ok(false);
                    }
                }
                Stage::Health if lifecycle => {
                    match self.scan_input(opts.time_budget.is_some())? {
                        Some((detection, mark)) => {
                            *input = Some(mark);
                            self.maintain_lifecycle(report, detection)?;
                        }
                        None => {
                            // Too much changed since the scan, or this handle
                            // did not read it: scan (again) in chunks.
                            let scan = self.maintain_scan.borrow();
                            if scan.is_none() {
                                cursor.lost_scans += 1;
                            }
                            cursor.stage = Stage::Scan;
                            cursor.offset = scan.as_ref().map_or(0, |s| s.next);
                            cursor.generation = scan.as_ref().map(|s| s.mark.generation());
                            drop(scan);
                            at = STAGES
                                .iter()
                                .position(|s| *s == Stage::Scan)
                                .unwrap_or_default();
                            continue;
                        }
                    }
                }
                Stage::Procedures => self.maintain_procedures(report)?,
                _ => {}
            }
            at += 1;
        }
        Ok(true)
    }

    fn load_cursor(&self) -> Option<Cursor> {
        let bytes = std::fs::read(self.local_dir().join(CURSOR_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Record (or, with `None`, forget) where the next pass resumes.
    /// Best effort: a lost cursor only makes the next pass start over.
    fn save_cursor(&self, cursor: Option<&Cursor>, exists: bool) {
        let file = self.local_dir().join(CURSOR_FILE);
        match cursor {
            Some(cursor) => {
                let Ok(text) = serde_json::to_vec(cursor) else {
                    return;
                };
                if let Some(dir) = file.parent()
                    && std::fs::create_dir_all(dir).is_ok()
                {
                    let _ =
                        wi_history::durable::write_atomic(&file, &text, self.history.durability());
                }
            }
            None if exists => {
                let _ = std::fs::remove_file(&file);
            }
            None => {}
        }
    }

    fn manifest_pending(&self) -> bool {
        wi_history::Manifest::snapshot_unlocked(&self.history)
            .map_or(true, |snap| !snap.entries.is_empty())
    }

    /// Retry intents the last recovery held open, then remove engine temp
    /// files a crashed writer left (dead pid, no open intent, no
    /// `recovery-problem`): they hold only unacknowledged engine bytes.
    fn maintain_recovery(&self, report: &mut MaintainReport) -> Result<()> {
        let before = self.held.borrow().len();
        if before > 0 {
            let open = self.recover()?;
            let after = self.held.borrow().len();
            report.repaired += before.saturating_sub(after) + open.index_repaired.len();
        }
        report.repaired += self.remove_dead_temps()?.len();
        Ok(())
    }

    /// Repair a pending index; rebuild an unusable one or one without a
    /// complete walk. Never asks anyone to run a command.
    fn maintain_index(&self, report: &mut MaintainReport, full: bool) -> Result<()> {
        if !self.index_pending().is_empty() {
            match self.repair_index() {
                Ok(paths) => report.repaired += paths.len(),
                Err(_) => {
                    self.close_index();
                    self.reindex()?;
                    report.index_rebuilt = true;
                    return Ok(());
                }
            }
        }
        let healthy = matches!(self.index_as_of(), Ok(Some(_)));
        if !healthy {
            self.close_index();
            self.reindex()?;
            report.index_rebuilt = true;
            return Ok(());
        }
        if full {
            let sync = self.catch_up()?;
            report.repaired += sync.added + sync.updated + sync.removed;
            // Deletions no watcher saw become owner commits.
            report.repaired += self.record_missing()?.len();
        }
        Ok(())
    }

    /// Move engine work notes out of the folder older vaults used (`work/`,
    /// or the old `work_dir` key) into `.wiki-island/work`. Only notes of
    /// type `work` move. Capture notes stay where they are, so path links
    /// and source pins to them keep working; new captures go to
    /// `.wiki-island/captures`.
    /// Inspects [`MIGRATE_CHUNK`] files at a time; false when the budget
    /// stopped it (`cursor.after` records the last file inspected).
    fn maintain_migrate(
        &self,
        report: &mut MaintainReport,
        cursor: &mut Cursor,
        over: &dyn Fn() -> bool,
    ) -> Result<bool> {
        let moves = [(
            self.config.legacy_work_dir(),
            self.config.work_dir(),
            "work",
        )];
        for (legacy, current, kind) in moves {
            let dir = self.root().join(&legacy);
            if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let mut inspected = 0;
            for rel in crate::ingest::walk_files_in(self.root(), &dir) {
                if cursor.after.as_deref().is_some_and(|a| rel.as_str() <= a) {
                    continue;
                }
                if inspected >= MIGRATE_CHUNK && over() {
                    return Ok(false);
                }
                inspected += 1;
                cursor.after = Some(rel.clone());
                let Some(rest) = rel
                    .strip_prefix(legacy.as_str())
                    .and_then(|r| r.strip_prefix('/'))
                else {
                    continue;
                };
                if !crate::paths::is_markdown(&rel) || !note_has_type(self, &rel, kind) {
                    continue;
                }
                let to = format!("{current}/{rest}");
                if read_rel(self.root(), &to)?.is_some() {
                    continue;
                }
                self.move_note(MoveRequest {
                    from: rel,
                    to,
                    expected_checksum: None,
                    op_id: None,
                    ingress: Ingress::Engine,
                })?;
                report.migrated += 1;
            }
            remove_empty_dirs(&dir);
        }
        report.migrated += self.retire_legacy_exports()?;
        Ok(true)
    }

    /// Write the built-in maintenance procedure when it is missing or its
    /// bytes differ from the binary's text.
    fn maintain_builtin(&self, report: &mut MaintainReport) -> Result<()> {
        let path = format!(
            "{}/{}",
            wi_note::PROCEDURES_DIR,
            wi_lifecycle::maintenance::file_name()
        );
        let want = wi_lifecycle::maintenance::note_bytes().into_bytes();
        let current = read_rel(self.root(), &path)?;
        if current.as_deref() == Some(want.as_slice()) {
            return Ok(());
        }
        let expect = current.as_deref().map(wi_history::hash_bytes);
        self.publish_engine_bytes(&path, want, expect, Ingress::Engine, None)?;
        report.procedures_changed = true;
        Ok(())
    }

    /// Read the notes the detectors compare, [`SCAN_CHUNK`] at a time;
    /// false when the budget stopped the scan (`cursor.offset` records how
    /// far it got). A scan this handle did not start, or one older than
    /// the cursor, starts over; a resumed one first re-reads what changed.
    fn maintain_read_notes(&self, cursor: &mut Cursor, over: &dyn Fn() -> bool) -> Result<bool> {
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
        let snapshot = self.lifecycle_snapshot(&[])?;
        loop {
            cursor.offset = scan.next;
            cursor.generation = Some(scan.mark.generation());
            if scan.next >= scan.paths.len() {
                return Ok(true);
            }
            let end = (scan.next + SCAN_CHUNK).min(scan.paths.len());
            self.read_scan_paths(&snapshot, scan, end)?;
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
    fn read_scan_paths(&self, snapshot: &Snapshot, scan: &mut Scan, end: usize) -> Result<()> {
        for path in &scan.paths[scan.next..end] {
            match self.detection_note(snapshot, path)? {
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
    fn scan_input(
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
        let snapshot = self.lifecycle_snapshot(&[])?;
        let end = scan.paths.len();
        self.read_scan_paths(&snapshot, scan, end)?;
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

    /// Detector input for one path; none for work notes and unreadable or
    /// vanished files.
    fn detection_note(
        &self,
        snapshot: &Snapshot,
        path: &str,
    ) -> Result<Option<wi_lifecycle::detect::DetectionNote>> {
        if snapshot
            .get(path)
            .is_some_and(|n| n.lifecycle.lifecycle_type == wi_note::LifecycleType::Work)
        {
            return Ok(None);
        }
        // An unreadable file has its own ingest-failed item; it never stops the pass.
        Ok(read_rel(self.root(), path)
            .ok()
            .flatten()
            .and_then(|bytes| Note::parse(&bytes).ok())
            .map(|parsed| wi_lifecycle::detect::DetectionNote::from_note(path, &parsed)))
    }

    /// Procedure health and detector items, and closing items whose cause
    /// is gone.
    fn maintain_lifecycle(
        &self,
        report: &mut MaintainReport,
        detection: Vec<wi_lifecycle::detect::DetectionNote>,
    ) -> Result<()> {
        let base = self.lifecycle_snapshot(&[])?;
        let evidence = self.evidence_for(base.notes().iter().filter(|n| n.is_memory()));
        let snapshot = Snapshot::new(base.notes().to_vec(), evidence);
        let ctx = Context::new(&snapshot, self.lifecycle_now());
        let policy = self.lifecycle_policy();
        let engine = self.lifecycle_engine();
        let mut sizes = BTreeMap::new();
        for note in snapshot
            .notes()
            .iter()
            .filter(|n| n.lifecycle.kind == Some(wi_note::Kind::Procedure))
        {
            if let Some(bytes) = read_rel(self.root(), &note.path)? {
                // The same spans a delivery attaches to the hit.
                let fresh = wi_lifecycle::NoteSnapshot::from_bytes(note.path.as_str(), &bytes);
                let spans = if fresh.is_memory() {
                    engine.attached_spans(&fresh, &ctx)
                } else {
                    Vec::new()
                };
                sizes.insert(
                    note.path.clone(),
                    wi_lifecycle::delivery_size_with_spans(&note.path, &bytes, &spans),
                );
            }
        }
        let mut health = wi_lifecycle::procedure_health(&ctx, &policy, &sizes);
        // Detector inputs are ephemeral: files remain canonical and the device
        // snapshot/index format does not gain title or body copies.
        health.extend(wi_lifecycle::detect::duplicates(
            &detection, &policy, ctx.now,
        ));
        health.extend(wi_lifecycle::detect::promote_candidates(
            &detection, &policy, ctx.now,
        ));
        let mut existing = BTreeMap::new();
        for note in snapshot
            .notes()
            .iter()
            .filter(|n| n.lifecycle.lifecycle_type == wi_note::LifecycleType::Work)
        {
            if let Some(bytes) = read_rel(self.root(), &note.path)?
                && let Ok(parsed) = Note::parse(&bytes)
                && let Some(item) = self.engine_work_item(&note.path, &parsed)
            {
                existing.insert(item.id.clone(), (note.path.clone(), item));
            }
        }
        let active: BTreeSet<_> = health.iter().map(|item| item.id.as_str()).collect();
        let mut open_candidates = existing
            .values()
            .filter(|(_, item)| {
                item.kind == wi_lifecycle::WorkKind::PromoteCandidate
                    && item.state != State::Closed
                    && active.contains(item.id.as_str())
            })
            .count();
        let mut promoted = 0;
        let mut duplicated = 0;
        for item in &health {
            match existing.get(&item.id) {
                None => {
                    if item.kind == wi_lifecycle::WorkKind::PromoteCandidate {
                        if promoted >= wi_lifecycle::detect::MAX_NEW_CANDIDATES
                            || open_candidates >= wi_lifecycle::detect::MAX_OPEN_CANDIDATES
                        {
                            continue;
                        }
                        promoted += 1;
                        open_candidates += 1;
                    }
                    if item.kind == wi_lifecycle::WorkKind::Duplicate {
                        if duplicated >= wi_lifecycle::detect::MAX_NEW_DUPLICATES {
                            continue;
                        }
                        duplicated += 1;
                    }
                    self.write_work(item)?;
                    report.work_created += 1;
                }
                Some((path, old)) => {
                    let reopens = old.state == State::Closed
                        && old
                            .reopen
                            .as_ref()
                            .is_some_and(|r| r.trigger == item.next_trigger)
                        && old
                            .condition
                            .as_deref()
                            .is_some_and(|c| item.condition.as_deref() != Some(c));
                    if item.kind == wi_lifecycle::WorkKind::PromoteCandidate && reopens {
                        if open_candidates >= wi_lifecycle::detect::MAX_OPEN_CANDIDATES {
                            continue;
                        }
                        open_candidates += 1;
                    }
                    self.refresh_sweep_item(path, item)?;
                }
            }
        }
        // Everything a sweep would produce now, by work id.
        let swept = engine.sweep(&ctx);
        let live: BTreeSet<String> = swept
            .create
            .iter()
            .chain(swept.refresh.iter().map(|(_, i)| i))
            .chain(health.iter())
            .map(|i| i.id.clone())
            .collect();
        let head = wi_history::History::head(&self.history)?;
        for note in snapshot.notes() {
            if note.lifecycle.lifecycle_type != wi_note::LifecycleType::Work {
                continue;
            }
            let Some(bytes) = read_rel(self.root(), &note.path)? else {
                continue;
            };
            let parsed = Note::parse(&bytes)?;
            let Some(item) = self.engine_work_item(&note.path, &parsed) else {
                continue;
            };
            if item.state == State::Closed {
                continue;
            }
            let swept_gone = wi_lifecycle::auto_closable(&item) && !live.contains(&item.id);
            if !swept_gone && !self.cause_gone(&item, &snapshot, head.as_deref())? {
                continue;
            }
            let resolved = WorkItem {
                condition: Some("resolved".into()),
                ..item.clone()
            };
            let reopen = wi_lifecycle::Reopen {
                trigger: item.next_trigger,
                detail: "reopens when the maintenance pass sees the cause again".into(),
            };
            let closed = resolved
                .close(Outcome::Resolved, None, Some(reopen))
                .map_err(|e| StoreError::Invalid(e.to_string()))?;
            if self.refresh_work(&note.path, &closed)? {
                report.work_closed += 1;
            }
        }
        Ok(())
    }

    /// Whether the cause of an open engine item is gone although no sweep
    /// re-derives it: a gate, conflict, supersedes or deferred item whose
    /// target is no longer a candidate claim (adopted, rejected, archived,
    /// made plain or deleted); an origin-rule item whose target was
    /// adopted or is gone; a `missing` item whose file is back or whose
    /// deletion history now records; an `ingest-failed` item whose file
    /// is gone or is in history with its current bytes.
    fn cause_gone(&self, item: &WorkItem, snapshot: &Snapshot, head: Option<&str>) -> Result<bool> {
        use wi_lifecycle::WorkKind;
        let target = snapshot.get(&item.target);
        let candidate = target.is_some_and(|n| {
            n.is_memory() && matches!(n.lifecycle.status, None | Some(wi_note::Status::Candidate))
        });
        Ok(match item.kind {
            WorkKind::Gate | WorkKind::InvalidSupersedes | WorkKind::Deferred => !candidate,
            // Only the conflict a failed adoption opened; slot conflicts and
            // converted conflict requests close by their own rules.
            WorkKind::Conflict => item.reason.starts_with(CANDIDATE_CONFLICT) && !candidate,
            WorkKind::OriginRule => target.is_none_or(|n| {
                !n.is_memory() || n.lifecycle.status == Some(wi_note::Status::Active)
            }),
            WorkKind::Recheck if item.related.iter().any(|r| r.starts_with(MISSING_PREFIX)) => {
                let back = read_rel(self.root(), &item.target)?.is_some();
                let recorded = match head {
                    Some(head) => {
                        wi_history::History::blob_at(&self.history, head, &item.target)?.is_none()
                    }
                    None => true,
                };
                back || recorded
            }
            WorkKind::Recheck
                if item
                    .related
                    .iter()
                    .any(|r| r.starts_with(INGEST_FAILED_PREFIX)) =>
            {
                // Still unreadable: the cause is not gone.
                let Ok(current) = read_rel(self.root(), &item.target) else {
                    return Ok(false);
                };
                match (current, head) {
                    (None, _) => true,
                    (Some(_), None) => false,
                    (Some(bytes), Some(head)) => {
                        wi_history::History::blob_at(&self.history, head, &item.target)?
                            == Some(wi_history::blob_id_of(&bytes)?)
                    }
                }
            }
            _ => false,
        })
    }

    /// Whether any procedure note changed since the last pass on this
    /// device; remembers the current set.
    fn maintain_procedures(&self, report: &mut MaintainReport) -> Result<()> {
        let notes = self.lifecycle_notes()?;
        let procedures: Vec<_> = notes
            .iter()
            .filter(|n| n.lifecycle.kind == Some(wi_note::Kind::Procedure))
            .collect();
        let digest = wi_lifecycle::sha256(
            serde_json::to_string(&procedures)
                .map_err(|e| StoreError::Invalid(e.to_string()))?
                .as_bytes(),
        );
        let file = self.history.wi_dir().join(STATE_FILE);
        let mut state: DeviceState = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if state.procedures.as_deref() == Some(digest.as_str()) {
            return Ok(());
        }
        report.procedures_changed = true;
        state.procedures = Some(digest);
        let text = serde_json::to_vec(&state).map_err(|e| StoreError::Invalid(e.to_string()))?;
        wi_history::durable::write_atomic(&file, &text, self.history.durability())?;
        Ok(())
    }
}

fn note_has_type(vault: &Vault, rel: &str, kind: &str) -> bool {
    read_rel(vault.root(), rel)
        .ok()
        .flatten()
        .and_then(|bytes| Note::parse(&bytes).ok())
        .and_then(|note| {
            note.frontmatter()
                .and_then(|f| f.get("type"))
                .and_then(|v| v.as_str().map(str::to_owned))
        })
        .is_some_and(|t| t == kind)
}

/// Remove `dir` and its subdirectories when they hold nothing.
fn remove_empty_dirs(dir: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                remove_empty_dirs(&entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

#[cfg(test)]
mod tests {
    use crate::{Ingress, OpenOptions, Vault, WriteRequest};

    fn write(vault: &Vault, path: &str, text: &str, ingress: Ingress) {
        vault
            .write(WriteRequest {
                path: path.into(),
                content: text.as_bytes().to_vec(),
                expected_checksum: None,
                op_id: None,
                ingress,
            })
            .unwrap();
    }

    #[test]
    fn only_a_note_the_detectors_read_makes_the_input_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let (vault, _) = Vault::init_with(
            dir.path().join("vault"),
            OpenOptions {
                index_path: Some(dir.path().join("index.sqlite3")),
            },
        )
        .unwrap();
        let mark = vault.publication_mark().unwrap();
        assert!(!vault.input_stale(&mark).unwrap());
        let work = format!("{}/item.md", vault.config.work_dir());
        write(
            &vault,
            &work,
            "---\ntype: work\n---\nitem\n",
            Ingress::Engine,
        );
        assert!(!vault.input_stale(&mark).unwrap(), "own work notes");
        write(&vault, "note.md", "changed after the input", Ingress::Cli);
        assert!(vault.input_stale(&mark).unwrap());
    }
}
