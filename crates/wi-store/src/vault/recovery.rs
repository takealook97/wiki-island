//! Crash recovery and intent settlement.

use wi_history::{HistoryError, Manifest, OpRecord, PublisherLock, Recovery, hash_bytes};

use crate::contained::Dir;
use crate::error::{Result, StoreError};
use crate::opresult;
use crate::paths::{MOVED_MARK, TMP_MARK, Target};
use crate::work::{self, WorkKind, WorkRequest};

use super::{ContainedTree, OpenReport, Vault, conflict_copy_names, free_conflict_name, rel_in};

impl Vault {
    /// Replay the manifest: clean up after a crashed write, settle its
    /// intent, flush, record work requests, repair a pending index. Index
    /// repair never fails the open: it is reported in `index_degraded`. A
    /// problem with one path of the open intent (unreadable, unexpected
    /// kind, failed rename) never fails it either: the intent stays open
    /// with one `recovery-problem` work request per path, and a later open
    /// retries it. Only writes to that intent's paths wait for it
    /// (`recovery-held`); writes to other paths, flushes and commits go on.
    pub fn recover(&self) -> Result<OpenReport> {
        self.recover_sync()?;
        let prepared = self.prepare_open_intent()?;
        let mut report = OpenReport {
            temp_cleanup: prepared.notes,
            ..OpenReport::default()
        };
        let tree = ContainedTree {
            root: self.root(),
            hold: &prepared.hold,
        };
        report.recovery = Recovery::replay(&self.history, &tree)?;
        *self.held.borrow_mut() = report.recovery.unsettled.iter().map(|u| u.seq).collect();
        for unsettled in &report.recovery.unsettled {
            // A held intent already has its work request.
            if !prepared.hold.contains(&unsettled.path) {
                report.temp_cleanup.push(self.recovery_problem(
                    &unsettled.path,
                    unsettled.seq,
                    &unsettled.error,
                )?);
            }
        }
        if self.recover_import_batches()? {
            report.recovery.flush = self.flush()?;
        }
        for (seq, op_id) in &prepared.open {
            if let Some(op_id) = op_id
                && report.recovery.dropped.contains(seq)
            {
                opresult::remove(&self.history, op_id)?;
            }
        }
        for conflict in &report.recovery.conflicts {
            let request = WorkRequest::new(
                WorkKind::RecoveryConflict,
                &conflict.path,
                format!(
                    "crash recovery found other bytes ({}); the engine version is blob {}",
                    conflict.current_hash.as_deref().unwrap_or("absent"),
                    conflict.new_blob
                ),
            )
            .with_seq(conflict.seq);
            self.record_work(request.clone())?;
            report.work_requests.push(request);
            let guard = PublisherLock::acquire(&self.history)?;
            Manifest::ack_conflict(&self.history, &guard, conflict.seq)?;
        }
        if !report.recovery.conflicts.is_empty() {
            report.recovery.flush = self.flush()?;
        }
        for entry in &report.recovery.quarantined {
            let request = WorkRequest::new(
                WorkKind::Quarantine,
                entry.file.display().to_string(),
                format!(
                    "{} byte(s) at offset {}: {}",
                    entry.len, entry.offset, entry.reason
                ),
            );
            self.record_work(request.clone())?;
            report.work_requests.push(request);
        }
        match self.repair_index() {
            Ok(paths) => report.index_repaired = paths,
            Err(e) => {
                self.close_index();
                report.index_degraded = Some(e.to_string());
            }
        }
        Ok(report)
    }

    /// Record that recovery left a persisted path untouched: one request
    /// per (sequence id, path), however often the open is retried.
    fn recovery_problem(
        &self,
        path: &str,
        seq: u64,
        error: &dyn std::fmt::Display,
    ) -> Result<String> {
        let detail = format!("crash recovery left {path} untouched: {error}");
        let known = work::list(&self.history)?
            .iter()
            .any(|w| w.kind == WorkKind::RecoveryProblem && w.path == path && w.seq == Some(seq));
        if !known {
            self.record_work(
                WorkRequest::new(WorkKind::RecoveryProblem, path, detail.clone()).with_seq(seq),
            )?;
        }
        Ok(detail)
    }

    /// Record a conflict-copy request unless one exists for (path, seq).
    pub(crate) fn conflict_work_once(&self, path: &str, seq: u64, detail: String) -> Result<bool> {
        let known = work::list(&self.history)?
            .iter()
            .any(|w| w.kind == WorkKind::ConflictCopy && w.path == path && w.seq == Some(seq));
        if !known {
            self.record_work(WorkRequest::new(WorkKind::ConflictCopy, path, detail).with_seq(seq))?;
        }
        Ok(!known)
    }

    /// Conflict copies of `target` for `seq` that already exist (a writer
    /// crashed between renaming one and recording its work request) get
    /// their request now.
    fn announce_conflict_copies(
        &self,
        dir: &Dir,
        target: &Target,
        seq: u64,
    ) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        for name in conflict_copy_names(dir, target, seq)? {
            let rel = rel_in(&target.rel, &name);
            if self.conflict_work_once(
                &target.rel,
                seq,
                format!("bytes kept by interrupted engine write {seq} at {rel}"),
            )? {
                notes.push(format!("announced {rel}"));
            }
        }
        Ok(notes)
    }

    /// Before replay settles a crashed write: remove or keep its temp
    /// files and the source of a completed move. Runs under the publisher
    /// lock, so no live writer can hold an open intent. Every persisted
    /// path is opened through checked directories; a path whose directory
    /// became a symlink (or a non-directory) is left untouched with a
    /// `recovery-problem` work request. Any other file-system error on one
    /// of the intent's paths holds the intent open (see `recover`).
    fn prepare_open_intent(&self) -> Result<Prepared> {
        let mut out = Prepared::default();
        let guard = PublisherLock::acquire(&self.history)?;
        let snap = match Manifest::snapshot(&self.history, &guard) {
            Ok(snap) => snap,
            // Replay quarantines the corrupt tail first.
            Err(HistoryError::CorruptManifest { .. }) => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in snap.open_intents() {
            if let OpRecord::Intent(rec) = &entry.op {
                self.prepare_one(entry.seq, rec, &mut out)?;
            }
        }
        drop(guard);
        Ok(out)
    }

    /// `prepare_open_intent` for one open intent.
    fn prepare_one(
        &self,
        seq: u64,
        rec: &wi_history::IntentRecord,
        out: &mut Prepared,
    ) -> Result<()> {
        out.open.push((seq, rec.op_id.clone()));
        let target = Target {
            abs: self.root().join(&rec.path),
            rel: rec.path.clone(),
        };
        let mut held = false;
        let (dir, name) = match Dir::open_parent(self.root(), &rec.path) {
            Ok(Some(found)) => found,
            Ok(None) => return Ok(()),
            Err(e) => {
                out.notes.push(self.recovery_problem(&rec.path, seq, &e)?);
                if matches!(e, StoreError::Io { .. }) {
                    out.hold.insert(rec.path.clone());
                }
                return Ok(());
            }
        };
        match self.settle_temps(&dir, &name, &target, rec, seq) {
            Ok((notes, problems)) => {
                out.notes.extend(notes);
                for (path, e) in problems {
                    out.notes.push(self.recovery_problem(&path, seq, &e)?);
                    held = true;
                }
            }
            Err(e) => {
                out.notes.push(self.recovery_problem(&rec.path, seq, &e)?);
                held = true;
            }
        }
        out.notes
            .extend(self.announce_conflict_copies(&dir, &target, seq)?);
        let current = match dir.read_regular(&name) {
            Ok(bytes) => bytes.map(|b| hash_bytes(&b)),
            Err(StoreError::Protected { .. } | StoreError::Invalid(_)) => None,
            Err(e) => {
                out.notes.push(self.recovery_problem(&rec.path, seq, &e)?);
                out.hold.insert(rec.path.clone());
                return Ok(());
            }
        };
        if let Some(from) = &rec.moved_from
            && !held
        {
            let placed = if current.as_deref() == Some(rec.new_hash.as_str()) {
                MoveTarget::Placed
            } else if current == rec.old_hash {
                MoveTarget::Undone
            } else {
                MoveTarget::Edited
            };
            match self.try_settle_move_source(from, &rec.new_hash, seq, placed) {
                Ok(notes) => out.notes.extend(notes),
                Err(e) => {
                    out.notes.push(self.recovery_problem(from, seq, &e)?);
                    held = true;
                }
            }
        }
        if held {
            out.hold.insert(rec.path.clone());
        }
        Ok(())
    }

    /// Temp files next to the open intent's target: remove the ones that
    /// hold its old or new bytes, keep any other as a conflict copy with a
    /// work request. A failure on one temp is returned with its path and
    /// the others are still settled.
    #[allow(clippy::type_complexity)]
    fn settle_temps(
        &self,
        dir: &Dir,
        name: &str,
        target: &Target,
        rec: &wi_history::IntentRecord,
        seq: u64,
    ) -> Result<(Vec<String>, Vec<(String, StoreError)>)> {
        let mut notes = Vec::new();
        let mut problems = Vec::new();
        let prefix = format!(".{name}{TMP_MARK}");
        for temp in dir.names()? {
            if !temp.starts_with(&prefix) {
                continue;
            }
            let temp_rel = rel_in(&target.rel, &temp);
            let bytes = match dir.read_regular(&temp) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(e) => {
                    problems.push((temp_rel, e));
                    continue;
                }
            };
            let hash = hash_bytes(&bytes);
            if Some(&hash) == rec.old_hash.as_ref() || hash == rec.new_hash {
                match dir.remove(&temp) {
                    Ok(()) => notes.push(format!("removed {temp}")),
                    Err(e) => problems.push((temp_rel, e)),
                }
                continue;
            }
            let copy = free_conflict_name(dir, target, seq)?;
            let rel = rel_in(&target.rel, &copy);
            if let Err(e) = dir.rename(&temp, dir, &copy) {
                problems.push((rel, e));
                continue;
            }
            self.conflict_work_once(
                &rec.path,
                seq,
                format!("bytes displaced by a crashed write kept at {rel}"),
            )?;
            notes.push(format!("kept {rel}"));
        }
        dir.barrier(self.history.durability())?;
        Ok((notes, problems))
    }

    /// The source of a crashed move: remove it if it still derives the
    /// target bytes, otherwise keep it as a conflict copy. A source the
    /// crashed writer had already taken aside (`.<name>.wi-moved-<seq>`) is
    /// always settled; the live source name only when the target holds the
    /// moved bytes, and then it is taken aside first, so an editor's atomic
    /// save there is never removed. When the move was undone or never
    /// placed (the target has its old bytes), the aside goes back under its
    /// name, or is kept as a conflict copy if that name is taken again.
    fn try_settle_move_source(
        &self,
        from: &str,
        new_hash: &str,
        seq: u64,
        state: MoveTarget,
    ) -> Result<Vec<String>> {
        let source = Target {
            abs: self.root().join(from),
            rel: from.to_owned(),
        };
        let Some((dir, name)) = Dir::open_parent(self.root(), from)? else {
            return Ok(Vec::new());
        };
        let aside = source.sibling_name(MOVED_MARK, &seq.to_string());
        let mut notes = Vec::new();
        let taken = match dir.read_regular(&aside)? {
            Some(_) => true,
            None if state == MoveTarget::Placed => dir.rename(&name, &dir, &aside)?,
            None => false,
        };
        if taken {
            let bytes = dir.read_regular(&aside)?.unwrap_or_default();
            if state != MoveTarget::Undone && derives_move_target(&bytes, from, new_hash) {
                dir.remove(&aside)?;
                notes.push(format!("removed move source {from}"));
            } else if state == MoveTarget::Undone
                && crate::exchange::rename_no_replace(&dir, &aside, &name)?
            {
                notes.push(format!("restored move source {from}"));
            } else {
                let copy = free_conflict_name(&dir, &source, seq)?;
                dir.rename(&aside, &dir, &copy)?;
                let rel = rel_in(from, &copy);
                self.conflict_work_once(
                    from,
                    seq,
                    format!("move source edited during a crashed move kept at {rel}"),
                )?;
                notes.push(format!("kept {rel}"));
            }
            dir.barrier(self.history.durability())?;
        }
        notes.extend(self.announce_conflict_copies(&dir, &source, seq)?);
        Ok(notes)
    }
}

/// What `prepare_open_intent` did and whether the intent stays open.
#[derive(Default)]
struct Prepared {
    notes: Vec<String>,
    /// Each open intent's sequence id and operation id.
    open: Vec<(u64, Option<String>)>,
    /// Target paths of intents with a problem on one of their paths:
    /// replay leaves them open so a later open retries.
    hold: std::collections::BTreeSet<String>,
}

/// Where a crashed move's target stands when its source is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveTarget {
    /// The target holds the moved bytes.
    Placed,
    /// The target has its old bytes again: the move did not happen.
    Undone,
    /// The target holds a third version (an editor saved it).
    Edited,
}

/// Whether `bytes` at the move source `from` are what the move placed at
/// its target (`new_hash`): a note gains its old permalink when it has
/// none; an attachment moves byte for byte. A note that no longer parses
/// was edited after the move began.
fn derives_move_target(bytes: &[u8], from: &str, new_hash: &str) -> bool {
    if crate::paths::is_markdown(from) {
        crate::ops::move_bytes(bytes, from).is_ok_and(|moved| hash_bytes(&moved) == new_hash)
    } else {
        hash_bytes(bytes) == new_hash
    }
}
