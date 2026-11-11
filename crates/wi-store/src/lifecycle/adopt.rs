//! Automatic adoption, the deferred queue, supersede recovery, conflict
//! propagation and the sweep.

use serde::Serialize;
use serde_json::{Value, json};
use wi_lifecycle::{
    ActiveCounts, AdoptionOutcome, Context, LossEvent, LossRequest, NoteEdit, NoteSnapshot,
    Snapshot, WorkItem,
};

use crate::error::{Result, StoreError};
use crate::fault;
use crate::lifecycle::Charge;
use crate::vault::{Vault, WriteResult};

/// What adoption did with one note.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AdoptReport {
    pub path: String,
    /// `active`, `candidate`, `deferred`, `rejected` or `unchanged`.
    pub outcome: String,
    /// Gate results (`pass` or `fail: <reason>`), when the gates ran.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub gates: Value,
    /// Rejection or unchanged reason.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub reason: Value,
    /// Notes marked `superseded`.
    pub superseded: Vec<String>,
    /// Work notes written for this outcome.
    pub work: Vec<String>,
    /// `budget-exhausted`: earliest retry time and the spent operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<String>,
    pub spent: usize,
    pub writes: usize,
}

/// What a sweep did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SweepSummary {
    pub resumed: Vec<AdoptReport>,
    pub recovered: Vec<String>,
    pub conflicts_flagged: Vec<String>,
    pub work_created: Vec<String>,
    pub work_refreshed: Vec<String>,
    pub requests_converted: Vec<String>,
}

fn gates_value(gates: &wi_lifecycle::GateReport) -> Value {
    Value::Object(
        gates
            .results
            .iter()
            .map(|(k, r)| (k.clone(), r.to_value()))
            .collect(),
    )
}

pub(crate) fn requests_of(losses: &[LossEvent]) -> Vec<LossRequest> {
    losses
        .iter()
        .map(|e| LossRequest {
            target: e.target.clone(),
            entities: e.entities.clone(),
            kind: e.kind,
        })
        .collect()
}

impl Vault {
    /// The policy-aware snapshot with evidence for `focus` and every note
    /// they name in `supersedes`.
    pub(crate) fn decision_snapshot(
        &self,
        fresh: &[NoteSnapshot],
        focus: &[&str],
    ) -> Result<Snapshot> {
        let base = self.lifecycle_snapshot(fresh)?;
        let mut named: Vec<&NoteSnapshot> = Vec::new();
        for path in focus {
            if let Some(note) = base.get(path) {
                named.push(note);
                named.extend(note.supersedes().iter().filter_map(|n| base.resolve(n)));
            }
        }
        let evidence = self.evidence_for(named);
        Ok(base.with_evidence(evidence))
    }

    /// Run the gates on `path` and apply the outcome. Lifecycle off, plain
    /// notes and non-candidates are `unchanged`.
    pub fn adopt(&self, path: &str) -> Result<AdoptReport> {
        self.adopt_with(path, None)
    }

    pub(crate) fn adopt_with(
        &self,
        path: &str,
        reserved: Option<&[LossEvent]>,
    ) -> Result<AdoptReport> {
        for attempt in 0..2 {
            match self.adopt_once(path, reserved) {
                // Another process spent the budget between the decision and
                // the charge: decide again with the new events.
                Err(StoreError::BudgetExhausted { .. } | StoreError::Stale { .. })
                    if attempt == 0 => {}
                other => return other,
            }
        }
        Err(StoreError::Invalid("adoption retry exhausted".into()))
    }

    fn adopt_once(&self, path: &str, reserved: Option<&[LossEvent]>) -> Result<AdoptReport> {
        let mut report = AdoptReport {
            path: path.to_owned(),
            outcome: "unchanged".into(),
            ..AdoptReport::default()
        };
        let Some((candidate, bytes)) = self.note_snapshot(path)? else {
            return Err(StoreError::NotFound {
                path: path.to_owned(),
            });
        };
        let hash = wi_history::hash_bytes(&bytes);
        let snapshot = self.decision_snapshot(std::slice::from_ref(&candidate), &[path])?;
        let ledger = self.loss_ledger()?;
        let now = self.lifecycle_now();
        let ctx = Context::new(&snapshot, now).with_ledger(&ledger);
        let ctx = match reserved {
            Some(events) => ctx.with_reserved(events),
            None => ctx,
        };
        let engine = self.lifecycle_engine();
        match engine.evaluate_adoption(&candidate, &ctx) {
            AdoptionOutcome::Unchanged { reason } => {
                report.reason = json!(reason);
            }
            AdoptionOutcome::Active {
                edits,
                gates,
                losses,
            } => {
                report.outcome = "active".into();
                report.gates = gates_value(&gates);
                let counts = ActiveCounts::from_snapshot(&snapshot, &engine.policy);
                let charge = (!losses.is_empty()).then(|| Charge {
                    requests: requests_of(&losses),
                    actor: losses[0].actor,
                    counts,
                    events: losses,
                });
                let mut edits = edits.into_iter();
                if let Some(first) = edits.next() {
                    report.writes +=
                        usize::from(self.apply_edit(&first, Some(&hash), charge)?.is_some());
                }
                // Read rule: an active note listed in another active note's
                // `supersedes` is never current, so a crash here is safe.
                fault::abort_at("lifecycle_after_candidate");
                for edit in edits {
                    if self.apply_edit(&edit, None, None)?.is_some() {
                        report.writes += 1;
                        report.superseded.push(edit.path.clone());
                    }
                }
            }
            AdoptionOutcome::Candidate { edits, gates, work } => {
                report.outcome = "candidate".into();
                report.gates = gates_value(&gates);
                self.apply_all(&edits, &hash, &work, &mut report)?;
            }
            AdoptionOutcome::Deferred {
                edits,
                gates,
                exhausted,
                work,
                ..
            } => {
                report.outcome = "deferred".into();
                report.gates = gates_value(&gates);
                report.retry_at = exhausted.retry_at.map(wi_lifecycle::format_time);
                report.spent = exhausted.spent.len();
                self.apply_all(&edits, &hash, &work, &mut report)?;
            }
            AdoptionOutcome::Rejected {
                reason,
                edits,
                work,
            } => {
                report.outcome = "rejected".into();
                report.reason = json!(reason);
                self.apply_all(&edits, &hash, &work, &mut report)?;
            }
        }
        Ok(report)
    }

    fn apply_all(
        &self,
        edits: &[NoteEdit],
        hash: &str,
        work: &[WorkItem],
        report: &mut AdoptReport,
    ) -> Result<()> {
        for (i, edit) in edits.iter().enumerate() {
            let expect = (i == 0 && edit.path == report.path).then_some(hash);
            report.writes += usize::from(self.apply_edit(edit, expect, None)?.is_some());
        }
        for item in work {
            report.work.push(self.write_work(item)?);
        }
        Ok(())
    }

    /// Adopt after an acknowledged write of a lifecycle note (any ingress).
    /// Returns the report as a JSON value for the write result, or `None`
    /// for plain notes and `lifecycle: off`.
    pub(crate) fn adopt_after_write(&self, result: &mut WriteResult) -> Result<()> {
        if !self.lifecycle_on() {
            return Ok(());
        }
        let Some((snap, _)) = self.note_snapshot(&result.path)? else {
            return Ok(());
        };
        if !snap.is_memory() {
            return Ok(());
        }
        let report = self.adopt(&result.path)?;
        let resumed = self.resume_deferred()?;
        let mut value = json!({"adoption": report});
        if !resumed.is_empty() {
            value["resumed"] = json!(resumed);
        }
        result.lifecycle = Some(merge(result.lifecycle.take(), value));
        Ok(())
    }

    /// Gates after a file ingress (editor save, scan) of a memory note.
    pub(crate) fn adopt_after_ingress(&self, path: &str) -> Result<Option<AdoptReport>> {
        if !self.lifecycle_on() {
            return Ok(None);
        }
        match self.note_snapshot(path)? {
            Some((snap, _)) if snap.is_memory() => match self.adopt(path) {
                Ok(report) => Ok(Some(report)),
                // The owner saved again meanwhile; the next event decides.
                Err(StoreError::Stale { .. } | StoreError::Locked { .. }) => Ok(None),
                Err(e) => Err(e),
            },
            _ => Ok(None),
        }
    }

    /// Flag conflicts between active claims (charged) and write their work.
    pub(crate) fn propagate_conflicts(&self) -> Result<Vec<String>> {
        let snapshot = self.lifecycle_snapshot(&[])?;
        let ledger = self.loss_ledger()?;
        let ctx = Context::new(&snapshot, self.lifecycle_now()).with_ledger(&ledger);
        let engine = self.lifecycle_engine();
        let plan = engine.propagate_conflicts(&ctx);
        let mut flagged = Vec::new();
        let counts = ActiveCounts::from_snapshot(&snapshot, &engine.policy);
        let mut charge = (!plan.losses.is_empty()).then(|| Charge {
            requests: requests_of(&plan.losses),
            actor: wi_lifecycle::Actor::Engine,
            counts,
            events: plan.losses.clone(),
        });
        for edit in &plan.edits {
            if self.apply_edit(edit, None, charge.take())?.is_some() {
                flagged.push(edit.path.clone());
            }
        }
        for item in &plan.work {
            self.write_work(item)?;
        }
        Ok(flagged)
    }

    /// The whole lifecycle pass: resume the queue, complete interrupted
    /// supersedes, flag conflicts, turn work requests into work notes and
    /// create or refresh recheck work items. It never changes a status
    /// except the supersede recovery and queued adoptions.
    pub fn sweep(&self) -> Result<SweepSummary> {
        let mut summary = SweepSummary::default();
        if !self.lifecycle_on() {
            return Ok(summary);
        }
        summary.resumed = self.resume_deferred()?;
        summary.recovered = self.recover_supersedes()?;
        summary.conflicts_flagged = self.propagate_conflicts()?;
        summary.requests_converted = self.convert_work_requests()?;
        self.sweep_items(&mut summary)?;
        Ok(summary)
    }

    /// The last sweep step: create or refresh recheck work items.
    pub(crate) fn sweep_items(&self, summary: &mut SweepSummary) -> Result<()> {
        let base = self.lifecycle_snapshot(&[])?;
        let evidence = self.evidence_for(base.notes().iter().filter(|n| n.is_memory()));
        let snapshot = base.with_evidence(evidence);
        let report = self
            .lifecycle_engine()
            .sweep(&Context::new(&snapshot, self.lifecycle_now()));
        for item in &report.create {
            summary.work_created.push(self.write_work(item)?);
        }
        for (path, item) in &report.refresh {
            if self.refresh_sweep_item(path, item)? {
                summary.work_refreshed.push(path.clone());
            }
        }
        Ok(())
    }

    /// Supersede recovery: mark the valid targets of active superseders `superseded`.
    pub fn recover_supersedes(&self) -> Result<Vec<String>> {
        let snapshot = self.lifecycle_snapshot(&[])?;
        let edits = self
            .lifecycle_engine()
            .recover_supersedes(&Context::new(&snapshot, self.lifecycle_now()));
        let mut done = Vec::new();
        for edit in edits {
            if self.apply_edit(&edit, None, None)?.is_some() {
                done.push(edit.path.clone());
            }
        }
        Ok(done)
    }
}

/// Merge two JSON objects (later keys win).
pub(crate) fn merge(base: Option<Value>, extra: Value) -> Value {
    match (base, extra) {
        (Some(Value::Object(a)), Value::Object(b)) => {
            Value::Object(a.into_iter().chain(b).collect())
        }
        (_, extra) => extra,
    }
}
