//! The deferred queue at execution time. A queue entry is executable only
//! when an engine operation wrote
//! its work note: the holder must be the engine-created deferred work item
//! for the entry, and its current bytes must be the ones the newest manifest
//! record or commit for that path published with the `engine` actor. Every
//! entry is then re-authorized against the live vault: entities come from
//! the live target, a changed target is refused, the rule and preference
//! retirement ban applies and the cap is checked again under the lock.

use serde_json::json;
use wi_history::{History, Manifest, OpStatus, TRAILER_ACTOR, blob_id_of, hash_bytes};
use wi_lifecycle::{
    ActiveCounts, Actor, DeferredAction, DeferredEntry, DeferredQueue, LossKind, Snapshot, WorkKind,
};
use wi_note::{Note, WorkState};

use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::lifecycle::AdoptReport;
use crate::lifecycle::revise::LossOutcome;
use crate::vault::Vault;

const ENGINE_ACTOR: &str = "engine";

/// What happened to one ready queue entry.
enum Resumed {
    /// Applied (or adopted): close the work note as adopted.
    Done(AdoptReport),
    /// Not allowed yet: the entry stays queued.
    Waiting(AdoptReport),
    /// Re-authorization refused it: close the work note with the reason.
    Refused(AdoptReport, String),
}

fn report(path: &str, outcome: &str) -> AdoptReport {
    AdoptReport {
        path: path.to_owned(),
        outcome: outcome.into(),
        ..AdoptReport::default()
    }
}

fn entry_target(entry: &DeferredEntry) -> Option<&str> {
    match &entry.action {
        DeferredAction::Adopt { candidate } => Some(candidate),
        DeferredAction::Loss => entry.requests.first().map(|r| r.target.as_str()),
    }
}

impl Vault {
    /// Whether the newest published change of `rel` is an engine write of
    /// exactly `bytes`: the latest unretired manifest record for the path,
    /// else the newest commit that changed it (`Wi-Actor: engine`).
    pub(crate) fn engine_written(&self, rel: &str, bytes: &[u8]) -> Result<bool> {
        let snap = Manifest::snapshot_unlocked(&self.history)?;
        let latest = snap
            .entries
            .iter()
            .rev()
            .find(|e| e.op.path() == rel || e.op.moved_from() == Some(rel));
        if let Some(entry) = latest {
            return Ok(entry.op.path() == rel
                && entry.status == OpStatus::Done
                && entry.op.actor() == ENGINE_ACTOR
                && entry.op.new_hash() == hash_bytes(bytes));
        }
        let blob = blob_id_of(bytes)?;
        let log = History::log(&self.history, rel)?;
        Ok(log.first().is_some_and(|commit| {
            commit.blob.as_ref() == Some(&blob)
                && commit
                    .trailers
                    .iter()
                    .any(|(k, v)| k == TRAILER_ACTOR && v == ENGINE_ACTOR)
        }))
    }

    /// The queue entries held by engine-created deferred work items, with
    /// requests rebuilt from the live targets (their entities are never
    /// taken from the stored entry), and the holder path and bytes for the
    /// provenance check at execution.
    fn bound_entries(&self, snapshot: &Snapshot) -> Result<Vec<(DeferredEntry, String, Vec<u8>)>> {
        let mut out = Vec::new();
        for note in snapshot.notes() {
            let Some(value) = note.deferred.clone() else {
                continue;
            };
            let open = note
                .lifecycle
                .work
                .as_ref()
                .is_none_or(|w| w.state != Some(WorkState::Closed));
            let Ok(entry) = serde_json::from_value::<DeferredEntry>(value) else {
                continue;
            };
            let Some(bytes) = read_rel(self.root(), &note.path)? else {
                continue;
            };
            let Ok(parsed) = Note::parse(&bytes) else {
                continue;
            };
            let Some(item) = self.engine_work_item(&note.path, &parsed) else {
                continue;
            };
            let bound = open
                && item.kind == WorkKind::Deferred
                && item.id == entry.id
                && item.deferred.as_ref() == Some(&entry)
                && entry_target(&entry) == Some(item.target.as_str());
            if !bound {
                continue;
            }
            let requests = entry
                .requests
                .iter()
                .map(|r| match snapshot.get(&r.target) {
                    Some(live) => wi_lifecycle::LossRequest {
                        entities: live.entity_ids(),
                        ..r.clone()
                    },
                    None => r.clone(),
                })
                .collect();
            out.push((
                DeferredEntry { requests, ..entry },
                note.path.clone(),
                bytes,
            ));
        }
        Ok(out)
    }

    /// Apply deferred-queue entries the window now allows, in queue order.
    pub fn resume_deferred(&self) -> Result<Vec<AdoptReport>> {
        if !self.lifecycle_on() {
            return Ok(Vec::new());
        }
        let snapshot = self.lifecycle_snapshot(&[])?;
        let bound = self.bound_entries(&snapshot)?;
        if bound.is_empty() {
            return Ok(Vec::new());
        }
        let queue = DeferredQueue::new(bound.iter().map(|(e, _, _)| e.clone()).collect());
        let policy = self.lifecycle_policy();
        let counts = ActiveCounts::from_snapshot(&snapshot, &policy);
        let ledger = self.loss_ledger()?;
        let resume = queue.resume(&ledger, &counts, self.lifecycle_now());
        let mut out = Vec::new();
        for (entry, events) in resume.ready {
            // An entry without engine provenance is data, not a command: it
            // stays visible as a work note but never executes.
            let holder = bound.iter().find(|(e, _, _)| e.id == entry.id);
            let trusted = match holder {
                Some((_, path, bytes)) => self.engine_written(path, bytes)?,
                None => false,
            };
            if !trusted {
                continue;
            }
            let resumed = match &entry.action {
                DeferredAction::Adopt { candidate } => {
                    let report = self.adopt_with(candidate, Some(&events))?;
                    if report.outcome == "deferred" {
                        Resumed::Waiting(report)
                    } else {
                        Resumed::Done(report)
                    }
                }
                DeferredAction::Loss => self.resume_loss(&entry)?,
            };
            let report = match resumed {
                Resumed::Done(report) => {
                    self.close_deferred_work(&snapshot, &entry.id, None)?;
                    report
                }
                Resumed::Waiting(report) => report,
                Resumed::Refused(report, reason) => {
                    self.close_deferred_work(&snapshot, &entry.id, Some(&reason))?;
                    report
                }
            };
            out.push(report);
        }
        Ok(out)
    }

    /// A deferred archive (or move into the archive) or conflict flag,
    /// re-authorized against the live target.
    fn resume_loss(&self, entry: &DeferredEntry) -> Result<Resumed> {
        let Some(first) = entry.requests.first() else {
            return Ok(Resumed::Refused(
                report("", "refused"),
                "queue entry without a request".into(),
            ));
        };
        let target = first.target.as_str();
        if !matches!(first.kind, LossKind::Archive | LossKind::MoveToArchive) {
            let flagged = self.propagate_conflicts()?;
            return Ok(Resumed::Done(AdoptReport {
                writes: flagged.len(),
                ..report(target, "applied")
            }));
        }
        let refused = |reason: String| {
            let mut out = report(target, "refused");
            out.reason = json!(reason);
            Ok(Resumed::Refused(out, reason))
        };
        let Some(bytes) = read_rel(self.root(), target)? else {
            return refused(format!("{target} no longer exists"));
        };
        if entry
            .target_hash
            .as_ref()
            .is_some_and(|h| h != &hash_bytes(&bytes))
        {
            return refused(format!("{target} changed after the change was queued"));
        }
        // Only an agent or the engine is ever capped; a stored `user` actor
        // is not trusted to lift the retirement ban.
        let actor = match entry.actor {
            Actor::Engine => Actor::Engine,
            _ => Actor::Agent,
        };
        match self.loss_outcome(target, first.kind, actor)? {
            LossOutcome::Refused(reason) => refused(reason),
            LossOutcome::Deferred { .. } => Ok(Resumed::Waiting(report(target, "waiting"))),
            LossOutcome::Allowed(charge) => match self.archive_charged(target, charge) {
                Ok(_) => Ok(Resumed::Done(AdoptReport {
                    writes: 1,
                    ..report(target, "applied")
                })),
                Err(StoreError::BudgetExhausted { .. }) => {
                    Ok(Resumed::Waiting(report(target, "waiting")))
                }
                Err(e) => Err(e),
            },
        }
    }
}
