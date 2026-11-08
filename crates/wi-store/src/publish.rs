//! The write path (OPERATIONS `#write-path`):
//!
//! 1. temp file in the target directory + barrier;
//! 2. durable git blob;
//! 3. publisher lock (taken before the checksum check, so the check is
//!    race-free across processes), operation-id lookup, checksum check,
//!    manifest `intent`;
//! 4. placement re-validates every checked path: a move source is claimed
//!    (taken aside with its checked bytes), then the target is exchanged,
//!    keeping displaced editor bytes as a conflict copy. A path that
//!    vanished or changed since the check makes the operation stale;
//! 5. `done` record = acknowledgment; release the lock;
//! 6. index update (`index: pending` on failure). Git commits happen in the
//!    flush, never here.

use wi_history::{
    Blob, HistoryError, IntentRecord, Manifest, ManifestSnapshot, OpRecord, OpStatus,
    PublisherGuard, PublisherLock, hash_bytes,
};

use crate::contained::Dir;
use crate::error::{Result, StoreError};
use crate::exchange::{Placed, TempFile, place};
use crate::fault;
use crate::opresult::{self, FirstResult};
use crate::paths::{CONFLICT_MARK, MOVED_MARK, Target};
use crate::vault::{IndexState, Ingress, Vault, WriteResult};
use crate::work::{WorkKind, WorkRequest};

/// What the target must hold for the write to proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Expect {
    Absent,
    Hash(String),
}

/// One prepared single-file change.
#[derive(Debug, Clone)]
pub(crate) struct Change {
    pub target: Target,
    pub bytes: Vec<u8>,
    pub expect: Expect,
    /// Move source and the hash it must still have.
    pub moved_from: Option<(Target, String)>,
    pub op_id: Option<String>,
    pub digest: String,
    pub ingress: Ingress,
    pub revert_of: Option<String>,
    /// Change-cap charge, re-checked against the durable loss events under
    /// the publisher lock and recorded in the intent (`Wi-Loss` trailers).
    pub charge: Option<crate::lifecycle::Charge>,
}

/// Stable digest of a request: tool, target and arguments.
pub(crate) fn request_digest(value: &serde_json::Value) -> String {
    hash_bytes(&serde_json::to_vec(value).unwrap_or_default())
}

/// The manifest id stored with the first result of a write placed in the
/// folder only (over the history size limit).
const OUTSIDE_HISTORY: &str = "outside-history";

fn mismatch(op_id: &str) -> StoreError {
    StoreError::OperationMismatch {
        op_id: op_id.to_owned(),
    }
}

impl Vault {
    /// Look up an operation id under the publisher lock: unretired manifest
    /// records, then the stored first result, then commit trailers (through
    /// the device index, or a history walk without one). Same digest → the
    /// first result; other digest → refused.
    pub(crate) fn lookup_operation(
        &self,
        guard: &PublisherGuard,
        snap: &ManifestSnapshot,
        op_id: &str,
        digest: &str,
    ) -> Result<Option<WriteResult>> {
        let _ = guard;
        for entry in &snap.entries {
            let OpRecord::Intent(rec) = &entry.op else {
                continue;
            };
            if rec.op_id.as_deref() != Some(op_id) {
                continue;
            }
            if rec.request_digest != digest {
                return Err(mismatch(op_id));
            }
            if matches!(entry.status, OpStatus::Done | OpStatus::Conflict { .. }) {
                let first = match opresult::load(&self.history, op_id)? {
                    Some(first) if first.seq == entry.seq => first,
                    _ => self.reconstruct(snap, op_id, entry.seq, rec)?,
                };
                return Ok(Some(self.replayed(snap, first)));
            }
        }
        let stored = opresult::load(&self.history, op_id)?;
        if let Some(first) = &stored {
            if first.request_digest != digest {
                return Err(mismatch(op_id));
            }
            // A write kept outside history (over the size limit) has no
            // manifest entry or commit; its stored result is the record.
            if first.manifest_id == OUTSIDE_HISTORY {
                return Ok(Some(self.replayed(snap, first.clone())));
            }
            let retired = first.manifest_id == snap.manifest_id
                && first.seq <= snap.retired_upto
                && snap.entry(first.seq).is_none();
            if retired {
                return Ok(Some(self.replayed(snap, first.clone())));
            }
        }
        let Some(hit) = self.committed_operation(op_id)? else {
            return Ok(None);
        };
        if hit.request_digest != digest {
            return Err(mismatch(op_id));
        }
        if let Some(first) = stored.filter(|f| Some(f.seq) == hit.seq) {
            return Ok(Some(self.replayed(snap, first)));
        }
        let checksum = match &hit.blob {
            Some(blob) => hash_bytes(&self.history.read_blob(blob)?),
            None => String::new(),
        };
        let seq = hit.seq.unwrap_or(0);
        let first = FirstResult {
            op_id: op_id.to_owned(),
            request_digest: hit.request_digest,
            manifest_id: String::new(),
            seq,
            conflict_copy: self.conflict_copy_of(&hit.path, seq)?,
            path: hit.path,
            checksum,
            previous_checksum: None,
            moved_from: hit.moved_from,
        };
        Ok(Some(self.replayed(snap, first)))
    }

    /// A first result rebuilt from an unretired intent record when no
    /// stored result exists (a crash between the swap and `done`).
    fn reconstruct(
        &self,
        snap: &ManifestSnapshot,
        op_id: &str,
        seq: u64,
        rec: &IntentRecord,
    ) -> Result<FirstResult> {
        Ok(FirstResult {
            op_id: op_id.to_owned(),
            request_digest: rec.request_digest.clone(),
            manifest_id: snap.manifest_id.clone(),
            seq,
            path: rec.path.clone(),
            checksum: rec.new_hash.clone(),
            previous_checksum: rec.old_hash.clone(),
            conflict_copy: self.conflict_copy_of(&rec.path, seq)?,
            moved_from: rec.moved_from.clone(),
        })
    }

    /// The conflict copy a write `seq` to `path` kept, from its work request.
    fn conflict_copy_of(&self, path: &str, seq: u64) -> Result<Option<String>> {
        let kept = self
            .work_requests()?
            .iter()
            .any(|r| r.kind == WorkKind::ConflictCopy && r.seq == Some(seq) && r.path == path);
        Ok(kept.then(|| {
            Target {
                rel: path.to_owned(),
                abs: self.root().join(path),
            }
            .rel_sibling(CONFLICT_MARK, &seq.to_string())
        }))
    }

    /// The replayed result: stable first-result fields plus the current
    /// generation and index state.
    fn replayed(&self, snap: &ManifestSnapshot, first: FirstResult) -> WriteResult {
        let pending = self.index_pending().iter().any(|p| p == &first.path);
        WriteResult {
            path: first.path,
            checksum: first.checksum,
            previous_checksum: first.previous_checksum,
            seq: first.seq,
            generation: snap.generation,
            index: if pending {
                IndexState::Pending
            } else {
                IndexState::Ok
            },
            replayed: true,
            conflict_copy: first.conflict_copy,
            moved_from: first.moved_from,
            lifecycle: None,
        }
    }

    /// Check an operation id before any precondition (a known id returns
    /// its first result even if the file changed since).
    pub(crate) fn replay_operation(
        &self,
        op_id: Option<&str>,
        digest: &str,
    ) -> Result<Option<WriteResult>> {
        let Some(op_id) = op_id else {
            return Ok(None);
        };
        let guard = PublisherLock::acquire(&self.history)?;
        if self.history.wi_dir().join("sync-intent.json").exists() {
            return Err(StoreError::Invalid(
                "Sync recovery is finishing; retry this save.".into(),
            ));
        }
        let snap = Manifest::snapshot(&self.history, &guard)?;
        self.lookup_operation(&guard, &snap, op_id, digest)
    }

    /// Admit a record on `paths` while intents are open. An intent this
    /// handle has not tried to settle, or one on these paths, asks the
    /// caller to recover first (`OpenIntent`). After that, an intent on
    /// these paths that recovery held open refuses the record with
    /// `recovery-held`; intents on other paths do not block it.
    pub(crate) fn admit(
        &self,
        snap: &ManifestSnapshot,
        paths: &[&str],
        may_recover: bool,
    ) -> Result<()> {
        let touching = snap.open_intent_touching(paths);
        if may_recover {
            let held = self.held.borrow();
            if let Some(open) =
                touching.or_else(|| snap.open_intents().find(|e| !held.contains(&e.seq)))
            {
                return Err(HistoryError::OpenIntent { seq: open.seq }.into());
            }
        }
        touching.map_or(Ok(()), |open| Err(held_error(open, paths)))
    }

    /// Before a write reads its paths: when an open intent changes one of
    /// them, try to settle it (recovery) once, then refuse with
    /// `recovery-held` if it is still open. Writes to other paths pass.
    pub(crate) fn held_gate(&self, paths: &[&str]) -> Result<()> {
        let snap = Manifest::snapshot_unlocked(&self.history)?;
        if snap.open_intent_touching(paths).is_none() {
            return Ok(());
        }
        self.recover()?;
        let snap = Manifest::snapshot_unlocked(&self.history)?;
        snap.open_intent_touching(paths)
            .map_or(Ok(()), |open| Err(held_error(open, paths)))
    }

    /// Publish one change through the write path.
    pub(crate) fn publish(&self, change: Change) -> Result<WriteResult> {
        self.recover_sync()?;
        if crate::paths::is_under(&change.target.rel, self.config.work_dir())
            && !self.owns_work_path(&change.target.rel)
        {
            return Err(StoreError::LifecycleRefused {
                code: "foreign-device-work",
                message: "This work item belongs to another device and is read-only here.".into(),
            });
        }
        if let Some((source, _)) = &change.moved_from
            && crate::paths::is_under(&source.rel, self.config.work_dir())
            && !self.owns_work_path(&source.rel)
            && source.rel.rsplit_once('/').map(|(dir, _)| dir) != Some(self.config.work_dir())
        {
            return Err(StoreError::LifecycleRefused {
                code: "foreign-device-work",
                message: "This work item belongs to another device and is read-only here.".into(),
            });
        }
        if change.bytes.len() as u64 > crate::sync::LIMIT {
            return self.publish_oversize(&change);
        }
        for attempt in 0..2 {
            match self.publish_once(&change, attempt == 0) {
                Err(StoreError::History(HistoryError::OpenIntent { .. })) if attempt == 0 => {
                    // A crashed writer left an intent: settle it, retry once.
                    self.recover()?;
                }
                other => return other,
            }
        }
        Err(StoreError::Invalid("write retry exhausted".into()))
    }

    /// A file larger than the history limit is placed in the folder only:
    /// no blob, intent or commit. Its managed exclude rule and one work
    /// request are durable before the bytes land, so no later ingest can
    /// add it to history. History keeps any earlier version as is.
    fn publish_oversize(&self, change: &Change) -> Result<WriteResult> {
        if change.moved_from.is_some() {
            return Err(StoreError::Invalid(
                "Files larger than 50 MB stay out of history; move this one in the folder.".into(),
            ));
        }
        let mode = self.history.durability();
        let target = &change.target;
        self.persist_large_excludes(std::slice::from_ref(&target.rel))?;
        let (dir, name) = Dir::create_parent(self.root(), &target.rel, mode)?;
        let permissions = dir.permissions(&name)?;
        let mut temp = TempFile::create(&dir, &name, &change.bytes, mode, permissions)?;
        let new_hash = hash_bytes(&change.bytes);
        let guard = PublisherLock::acquire(&self.history)?;
        if self.history.wi_dir().join("sync-intent.json").exists() {
            return Err(StoreError::Invalid(
                "Sync recovery is finishing; retry this save.".into(),
            ));
        }
        let snap = Manifest::snapshot(&self.history, &guard)?;
        if let Some(op_id) = &change.op_id
            && let Some(first) = self.lookup_operation(&guard, &snap, op_id, &change.digest)?
        {
            return Ok(first);
        }
        let old_hash = check_preconditions(change, &dir, &name, None)?;
        let mut conflict_copy = None;
        let mut recheck = None;
        if old_hash.as_deref() != Some(new_hash.as_str()) {
            let placed =
                place(&dir, &temp.name, &name, old_hash.as_deref()).map_err(|e| match e {
                    StoreError::Stale {
                        expected, current, ..
                    } => StoreError::Stale {
                        path: target.rel.clone(),
                        expected,
                        current,
                    },
                    other => other,
                })?;
            // The displaced bytes are kept when an editor changed them,
            // exactly as on the ordinary write path.
            (conflict_copy, recheck) =
                self.settle_displaced(&mut temp, placed, target, old_hash.as_deref(), 0)?;
            dir.barrier(mode)?;
        }
        if let Some(op_id) = &change.op_id {
            opresult::store(
                &self.history,
                &FirstResult {
                    op_id: op_id.clone(),
                    request_digest: change.digest.clone(),
                    manifest_id: OUTSIDE_HISTORY.into(),
                    seq: 0,
                    path: target.rel.clone(),
                    checksum: new_hash.clone(),
                    previous_checksum: old_hash.clone(),
                    conflict_copy: conflict_copy.clone(),
                    moved_from: None,
                },
            )?;
        }
        drop(guard);
        // Last look at displaced bytes right before the temp guard unlinks
        // them, as on the ordinary write path.
        if let Some(file) = recheck
            && let Some(late) =
                self.recheck_displaced(&mut temp, file, target, old_hash.as_deref(), 0)
        {
            conflict_copy = Some(late);
        }
        drop(temp);
        Ok(WriteResult {
            path: target.rel.clone(),
            checksum: new_hash,
            previous_checksum: old_hash,
            seq: 0,
            generation: self.generation()?,
            index: IndexState::Ok,
            replayed: false,
            conflict_copy,
            moved_from: None,
            lifecycle: None,
        })
    }

    fn publish_once(&self, change: &Change, may_recover: bool) -> Result<WriteResult> {
        let mode = self.history.durability();
        let target = &change.target;
        let (dir, name) = Dir::create_parent(self.root(), &target.rel, mode)?;
        let source_dir = match &change.moved_from {
            Some((source, _)) => {
                Some(Dir::open_parent(self.root(), &source.rel)?.ok_or_else(|| {
                    StoreError::NotFound {
                        path: source.rel.clone(),
                    }
                })?)
            }
            None => None,
        };
        // A replacement keeps the permission bits of the file it replaces;
        // a moved file keeps those of its source.
        let permissions = match dir.permissions(&name)? {
            Some(bits) => Some(bits),
            None => match &source_dir {
                Some((sdir, sname)) => sdir.permissions(sname)?,
                None => None,
            },
        };
        let mut temp = TempFile::create(&dir, &name, &change.bytes, mode, permissions)?;
        let new_hash = hash_bytes(&change.bytes);
        let new_blob = Blob::write_durable(&self.history, &change.bytes)?;

        let guard = PublisherLock::acquire(&self.history)?;
        if self.history.wi_dir().join("sync-intent.json").exists() {
            return Err(StoreError::Invalid(
                "Sync recovery is finishing; retry this save.".into(),
            ));
        }
        let snap = Manifest::snapshot(&self.history, &guard)?;
        let paths: Vec<&str> = std::iter::once(target.rel.as_str())
            .chain(change.moved_from.as_ref().map(|(s, _)| s.rel.as_str()))
            .collect();
        self.admit(&snap, &paths, may_recover)?;
        if let Some(op_id) = &change.op_id
            && let Some(first) = self.lookup_operation(&guard, &snap, op_id, &change.digest)?
        {
            return Ok(first);
        }
        let old_hash = check_preconditions(change, &dir, &name, source_dir.as_ref())?;

        let losses = match &change.charge {
            Some(charge) => self.charge_under_lock(&snap, charge)?,
            None => Vec::new(),
        };
        fault::abort_at("before_intent_fsync");
        let seq = Manifest::append_intent(
            &self.history,
            &guard,
            IntentRecord {
                op_id: change.op_id.clone(),
                request_digest: change.digest.clone(),
                actor: change.ingress.actor().to_owned(),
                path: target.rel.clone(),
                moved_from: change.moved_from.as_ref().map(|(s, _)| s.rel.clone()),
                old_hash: old_hash.clone(),
                new_hash: new_hash.clone(),
                new_blob: new_blob.clone(),
                revert_of: change.revert_of.clone(),
                batch: self.import_mark(change.ingress),
                losses,
            },
        )?;
        fault::abort_at("after_intent_before_swap");
        if let Some(hook) = &self.hooks.before_swap {
            hook(&target.abs);
        }
        // Placement re-validates every checked path atomically: the move
        // source is claimed (taken aside with the checked bytes) and the
        // target is exchanged or created against the checked state. A path
        // that vanished or changed since the check makes the whole
        // operation stale and nothing is changed.
        let claimed = match (&change.moved_from, &source_dir) {
            (Some((source, expected)), Some((sdir, _))) => {
                match self.claim_source(sdir, source, expected, seq) {
                    Ok(aside) => Some(aside),
                    Err(e) => {
                        let now = dir
                            .read_regular(&name)
                            .ok()
                            .flatten()
                            .map(|b| hash_bytes(&b));
                        Manifest::append_abort(&self.history, &guard, seq, now)?;
                        return Err(e);
                    }
                }
            }
            _ => None,
        };
        let placed = match place(&dir, &temp.name, &name, old_hash.as_deref()) {
            Ok(placed) => placed,
            Err(e) => {
                if let (Some(aside), Some((source, _)), Some((sdir, _))) =
                    (&claimed, &change.moved_from, &source_dir)
                {
                    self.release_source(sdir, source, aside, seq)?;
                }
                let now = dir
                    .read_regular(&name)
                    .ok()
                    .flatten()
                    .map(|b| hash_bytes(&b));
                Manifest::append_abort(&self.history, &guard, seq, now)?;
                // Errors name vault-relative paths, never the absolute one.
                return Err(match e {
                    StoreError::Stale {
                        expected, current, ..
                    } => StoreError::Stale {
                        path: target.rel.clone(),
                        expected,
                        current,
                    },
                    other => other,
                });
            }
        };
        let (conflict_copy, recheck) =
            self.settle_displaced(&mut temp, placed, target, old_hash.as_deref(), seq)?;
        dir.barrier(mode)?;
        if let (Some(aside), Some((source, expected)), Some((sdir, _))) =
            (&claimed, &change.moved_from, &source_dir)
        {
            self.retire_move_source(sdir, source, aside, expected, seq)?;
        }
        let moved_from = change.moved_from.as_ref().map(|(s, _)| s.rel.clone());
        if let Some(op_id) = &change.op_id {
            opresult::store(
                &self.history,
                &FirstResult {
                    op_id: op_id.clone(),
                    request_digest: change.digest.clone(),
                    manifest_id: snap.manifest_id.clone(),
                    seq,
                    path: target.rel.clone(),
                    checksum: new_hash.clone(),
                    previous_checksum: old_hash.clone(),
                    conflict_copy: conflict_copy.clone(),
                    moved_from: moved_from.clone(),
                },
            )?;
        }
        if let Some(hook) = &self.hooks.after_swap {
            hook(&target.abs);
        }
        fault::abort_at("after_swap_before_done");
        Manifest::append_done(&self.history, &guard, seq)?;
        drop(guard);
        fault::abort_at("after_done");

        let mut paths = vec![target.rel.as_str()];
        if let Some((source, _)) = &change.moved_from {
            paths.push(source.rel.as_str());
        }
        self.published
            .borrow_mut()
            .extend(paths.iter().map(|p| (*p).to_owned()));
        if let Some(copy) = &conflict_copy {
            self.published.borrow_mut().insert(copy.clone());
        }
        let index = self.index_update(&paths);
        // Last look at displaced bytes about to be unlinked: an editor may
        // have written to that inode through a descriptor it held open.
        let conflict_copy = match recheck {
            Some(file) => {
                let late =
                    self.recheck_displaced(&mut temp, file, target, old_hash.as_deref(), seq);
                if let Some(copy) = &late {
                    self.published.borrow_mut().insert(copy.clone());
                }
                late.or(conflict_copy)
            }
            None => conflict_copy,
        };
        drop(temp);
        Ok(WriteResult {
            path: target.rel.clone(),
            checksum: new_hash,
            previous_checksum: old_hash,
            seq,
            generation: seq,
            index,
            replayed: false,
            conflict_copy,
            moved_from,
            lifecycle: None,
        })
    }

    /// After a swap the temp name holds the displaced bytes: drop them when
    /// they are what the precondition saw, otherwise keep them as a
    /// conflict copy with a work request. Bytes that cannot be read are
    /// kept too (a rename needs no read permission); they are never left to
    /// the temp guard. Returns the conflict copy and, for bytes still to be
    /// dropped, a handle on them for a last check before the unlink.
    fn settle_displaced(
        &self,
        temp: &mut TempFile,
        placed: Placed,
        target: &Target,
        old_hash: Option<&str>,
        seq: u64,
    ) -> Result<(Option<String>, Option<std::fs::File>)> {
        if placed != Placed::Exchanged {
            temp.disarm();
            return Ok((None, None));
        }
        let read = temp
            .dir
            .open_regular(&temp.name)
            .and_then(|file| match file {
                Some(mut file) => {
                    let bytes = read_all(&mut file, &temp.dir, &temp.name)?;
                    Ok(Some((file, bytes)))
                }
                None => Ok(None),
            });
        let note = match read {
            Ok(Some((file, bytes))) if Some(hash_bytes(&bytes).as_str()) == old_hash => {
                return Ok((None, Some(file))); // the temp guard removes it later
            }
            Ok(None) => return Ok((None, None)),
            Ok(Some(_)) => {
                format!("an editor save displaced by engine write {seq} is kept at")
            }
            Err(e) => format!(
                "bytes displaced by engine write {seq} could not be read ({e}); they are kept unread at"
            ),
        };
        Ok((self.keep_displaced(temp, target, seq, &note), None))
    }

    /// Keep the displaced temp as a conflict copy with a work request. If
    /// the rename fails, the temp stays (disarmed) with a
    /// `recovery-problem` request instead. Never fails: the bytes are kept
    /// either way.
    fn keep_displaced(
        &self,
        temp: &mut TempFile,
        target: &Target,
        seq: u64,
        note: &str,
    ) -> Option<String> {
        temp.disarm();
        let renamed = crate::vault::free_conflict_name(&temp.dir, target, seq).and_then(|copy| {
            temp.dir.rename(&temp.name, &temp.dir, &copy)?;
            Ok(copy)
        });
        match renamed {
            Ok(copy) => {
                let rel = crate::vault::rel_in(&target.rel, &copy);
                let _ = self.record_work(
                    WorkRequest::new(WorkKind::ConflictCopy, &target.rel, format!("{note} {rel}"))
                        .with_seq(seq),
                );
                Some(rel)
            }
            Err(e) => {
                let rel = crate::vault::rel_in(&target.rel, &temp.name);
                let _ = self.record_work(
                    WorkRequest::new(
                        WorkKind::RecoveryProblem,
                        &rel,
                        format!("{note} {rel}: renaming it to a conflict copy failed: {e}"),
                    )
                    .with_seq(seq),
                );
                None
            }
        }
    }

    /// Re-hash displaced bytes through their open handle right before the
    /// temp guard unlinks them; keep them if they changed since the swap.
    fn recheck_displaced(
        &self,
        temp: &mut TempFile,
        mut file: std::fs::File,
        target: &Target,
        old_hash: Option<&str>,
        seq: u64,
    ) -> Option<String> {
        if let Some(hook) = &self.hooks.before_displaced_recheck {
            hook(&target.abs);
        }
        use std::io::Seek;
        let now = file
            .seek(std::io::SeekFrom::Start(0))
            .map_err(|e| StoreError::io(temp.dir.path().join(&temp.name), e))
            .and_then(|_| read_all(&mut file, &temp.dir, &temp.name));
        match now {
            Ok(bytes) if Some(hash_bytes(&bytes).as_str()) == old_hash => None,
            _ => self.keep_displaced(
                temp,
                target,
                seq,
                &format!(
                    "an editor save written to the file displaced by engine write {seq} is kept at"
                ),
            ),
        }
    }

    /// Claim a move source for placement: take it aside atomically and
    /// check that the aside holds the bytes the precondition saw. A source
    /// that is gone, or that changed, is stale; a changed source is put
    /// back first. Returns the aside name.
    fn claim_source(&self, dir: &Dir, source: &Target, expected: &str, seq: u64) -> Result<String> {
        let aside = source.sibling_name(MOVED_MARK, &seq.to_string());
        let stale = |current| StoreError::Stale {
            path: source.rel.clone(),
            expected: expected.to_owned(),
            current,
        };
        if !dir.rename(source.file_name(), dir, &aside)? {
            return Err(stale(None));
        }
        let now = dir.read_regular(&aside)?.map(|b| hash_bytes(&b));
        if now.as_deref() == Some(expected) {
            return Ok(aside);
        }
        self.release_source(dir, source, &aside, seq)?;
        Err(stale(now))
    }

    /// Put a claimed move source back under its name. If an editor created
    /// a new file there meanwhile, the claimed bytes are kept as a conflict
    /// copy instead; nothing is overwritten.
    fn release_source(&self, dir: &Dir, source: &Target, aside: &str, seq: u64) -> Result<()> {
        if !crate::exchange::rename_no_replace(dir, aside, source.file_name())? {
            self.keep_source_copy(dir, source, aside, seq, "a claimed move source is kept at")?;
        }
        dir.barrier(self.history.durability())
    }

    /// Remove a claimed move source after the target was placed, or keep it
    /// as a conflict copy if it changed after the claim (an editor writing
    /// through a descriptor it held open).
    fn retire_move_source(
        &self,
        dir: &Dir,
        source: &Target,
        aside: &str,
        expected: &str,
        seq: u64,
    ) -> Result<()> {
        let bytes = dir.read_regular(aside)?.unwrap_or_default();
        if hash_bytes(&bytes) == expected {
            dir.remove(aside)?;
        } else {
            self.keep_source_copy(
                dir,
                source,
                aside,
                seq,
                "an editor save of the move source is kept at",
            )?;
        }
        dir.barrier(self.history.durability())?;
        Ok(())
    }

    fn keep_source_copy(
        &self,
        dir: &Dir,
        source: &Target,
        aside: &str,
        seq: u64,
        note: &str,
    ) -> Result<()> {
        let copy = crate::vault::free_conflict_name(dir, source, seq)?;
        dir.rename(aside, dir, &copy)?;
        self.record_work(
            WorkRequest::new(
                WorkKind::ConflictCopy,
                &source.rel,
                format!("{note} {}", crate::vault::rel_in(&source.rel, &copy)),
            )
            .with_seq(seq),
        )?;
        Ok(())
    }
}

/// The checksum check under the publisher lock, the same for every
/// operation kind: the target must match `expect` and a move source must
/// still hold its checked bytes. Returns the target's current hash.
fn check_preconditions(
    change: &Change,
    dir: &Dir,
    name: &str,
    source_dir: Option<&(Dir, String)>,
) -> Result<Option<String>> {
    let old_hash = dir.read_regular(name)?.as_deref().map(hash_bytes);
    check_expect(&change.target, &change.expect, old_hash.as_deref())?;
    if let (Some((source, expected)), Some((sdir, sname))) = (&change.moved_from, source_dir) {
        let now = sdir.read_regular(sname)?.map(|b| hash_bytes(&b));
        if now.as_deref() != Some(expected.as_str()) {
            return Err(StoreError::Stale {
                path: source.rel.clone(),
                expected: expected.clone(),
                current: now,
            });
        }
    }
    Ok(old_hash)
}

fn check_expect(target: &Target, expect: &Expect, current: Option<&str>) -> Result<()> {
    match (expect, current) {
        (Expect::Absent, None) => Ok(()),
        (Expect::Absent, Some(current)) => Err(StoreError::Exists {
            path: target.rel.clone(),
            current: current.to_owned(),
        }),
        (Expect::Hash(expected), current) if current != Some(expected.as_str()) => {
            Err(StoreError::Stale {
                path: target.rel.clone(),
                expected: expected.clone(),
                current: current.map(str::to_owned),
            })
        }
        (Expect::Hash(_), _) => Ok(()),
    }
}

fn read_all(file: &mut std::fs::File, dir: &Dir, name: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| StoreError::io(dir.path().join(name), e))?;
    Ok(bytes)
}

/// `recovery-held` for the first of `paths` the open intent changes.
fn held_error(open: &wi_history::ManifestEntry, paths: &[&str]) -> StoreError {
    StoreError::RecoveryHeld {
        path: open
            .op
            .paths()
            .find(|p| paths.contains(p))
            .unwrap_or_else(|| open.op.path())
            .to_owned(),
        seq: open.seq,
    }
}
