use std::collections::{BTreeMap, BTreeSet};

use super::MaintainReport;
use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::vault::Vault;
use wi_lifecycle::{Context, Outcome, Snapshot, State, WorkItem};
use wi_note::Note;

/// Reason prefix of the conflict item a failed adoption opens.
const CANDIDATE_CONFLICT: &str = "candidate conflicts with an active claim";

/// `related` prefix of an item converted from a `missing` work request.
const MISSING_PREFIX: &str = "missing: ";

/// `related` prefix of an item converted from an `ingest-failed` request.
const INGEST_FAILED_PREFIX: &str = "ingest-failed: ";

impl Vault {
    /// Procedure health and detector items, and closing items whose cause
    /// is gone.
    pub(super) fn maintain_lifecycle(
        &self,
        report: &mut MaintainReport,
        detection: Vec<wi_lifecycle::detect::DetectionNote>,
    ) -> Result<()> {
        let base = self.lifecycle_snapshot(&[])?;
        let evidence = self.evidence_for(base.notes().iter().filter(|n| n.is_memory()));
        let snapshot = base.with_evidence(evidence);
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
}
