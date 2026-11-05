//! wi-lifecycle: the approval-free lifecycle engine.
//!
//! Pure and model-free: inputs are snapshots (notes, source bytes with
//! engine-recorded evidence times, loss events, `now`); outputs are
//! decisions, frontmatter edits and work items. No function here reads or
//! writes files, takes locks or touches the network. The host (the
//! integration package over `wi-store`) applies edits under the publisher
//! lock and records loss events durably.
//!
//! Spec: `docs/KNOWLEDGE.md`, `docs/AUTHORITY.md`, `docs/FORMAT.md`.
#![forbid(unsafe_code)]

mod attach;
pub mod budget;
pub mod capture;
pub mod complete;
pub mod conflict;
pub mod detect;
pub mod eligibility;
pub mod freshness;
pub mod gates;
pub mod health;
pub mod lint;
pub mod maintenance;
pub mod model;
pub mod queue;
pub mod revision;
pub mod sweep;
pub mod time;
pub mod work;

use std::collections::{BTreeMap, BTreeSet};

pub use budget::{
    ActiveCounts, BudgetDecision, BudgetExhausted, BudgetLedger, BudgetScope, LossEvent, LossKind,
    LossRequest, budget_check, entity_cap,
};
pub use capture::{FilterOutcome, FilterReason, capture_filter};
pub use complete::{
    DEFAULT_USER_SCOPE, complete_agent_defaults, complete_user_defaults, summary_from_body,
};
pub use conflict::{Conflict, InvalidTarget};
pub use eligibility::{AttachedSpan, Eligibility, ReadPartition, Withheld};
pub use freshness::{Freshness, FreshnessState};
pub use gates::{
    AdoptionOutcome, ConflictPlan, GateReport, GateResult, LossAction, LossDecision, NoteEdit,
    RejectReason, UnchangedReason, gate_guidance,
};
pub use health::{
    DELIVERY_RESERVE, MCP_MAX_BYTES, ProcedureProblem, delivery_size, delivery_size_with_spans,
    procedure_health, procedure_problems,
};
pub use lint::{LintCode, LintFinding};
pub use model::{
    Actor, Context, Evidence, LIFECYCLE_TYPES, MATERIALIZED_FORMAT, NoteSnapshot, Policy,
    ReadQuery, Snapshot, SourceState, sha256,
};
pub use queue::{DeferredAction, DeferredEntry, DeferredQueue, Resume};
pub use revision::RevisionError;
pub use sweep::{SweepReport, auto_closable};
pub use time::{IsoDuration, TimeError, Timestamp, format_time, parse_time};
pub use work::{Outcome, Reopen, State, Trigger, WorkError, WorkItem, WorkKind};

use wi_note::{Note, OriginKind};

/// The lifecycle engine for one vault policy. Every method is a pure function.
#[derive(Debug, Clone, Default)]
pub struct Engine {
    pub policy: Policy,
}

impl Engine {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }

    /// Run the gates on a candidate.
    pub fn evaluate_adoption(&self, candidate: &NoteSnapshot, ctx: &Context) -> AdoptionOutcome {
        gates::evaluate_adoption(candidate, ctx, &self.policy)
    }

    /// `current` read eligibility.
    pub fn eligibility(&self, note: &NoteSnapshot, ctx: &Context) -> Eligibility {
        eligibility::eligibility(note, ctx, &self.policy)
    }

    /// Partition a whole snapshot for a `current` read.
    pub fn partition_current(&self, ctx: &Context) -> ReadPartition {
        eligibility::partition_current(ctx, &self.policy)
    }

    /// Source spans delivered with a non-literal claim.
    pub fn attached_spans(&self, note: &NoteSnapshot, ctx: &Context) -> Vec<AttachedSpan> {
        eligibility::attached_spans(note, ctx)
    }

    pub fn freshness(&self, note: &NoteSnapshot, evidence: &Evidence, now: Timestamp) -> Freshness {
        freshness::freshness(&note.lifecycle, evidence, &self.policy, now)
    }

    pub fn effective_observed_at(
        &self,
        note: &NoteSnapshot,
        evidence: &Evidence,
    ) -> Result<Option<Timestamp>, TimeError> {
        freshness::effective_observed_at(&note.lifecycle, evidence)
    }

    /// Refuse a write whose `observed_at`, `checked_at` or observation lies after `now`.
    /// Inert with `lifecycle: off`.
    pub fn validate_write(
        &self,
        note: &NoteSnapshot,
        evidence: &Evidence,
        now: Timestamp,
    ) -> Result<(), FreshnessState> {
        if !self.policy.lifecycle_on {
            return Ok(());
        }
        match freshness::freshness(&note.lifecycle, evidence, &self.policy, now).state {
            state @ (FreshnessState::FutureTimestamp(_) | FreshnessState::Unparseable(_)) => {
                Err(state)
            }
            _ => Ok(()),
        }
    }

    pub fn validate_supersedes(
        &self,
        candidate: &NoteSnapshot,
        target: &NoteSnapshot,
        ctx: &Context,
    ) -> Result<(), InvalidTarget> {
        conflict::validate_supersedes(candidate, target, &ctx.snapshot.evidence, ctx.now)
    }

    /// The change cap for this policy; every action is allowed with `lifecycle: off`.
    /// `budget::budget_check` remains the unconditional arithmetic.
    pub fn budget_check(
        &self,
        requests: &[LossRequest],
        events: &[LossEvent],
        counts: &ActiveCounts,
        now: Timestamp,
    ) -> BudgetDecision {
        if !self.policy.lifecycle_on {
            return BudgetDecision::Allowed;
        }
        budget::budget_check(requests, events, counts, now)
    }

    /// Authorize an archive, move into the archive, exclude or supersede.
    pub fn authorize_loss(&self, action: &LossAction, ctx: &Context) -> LossDecision {
        gates::authorize_loss(action, ctx, &self.policy)
    }

    pub fn lint(&self, note: &NoteSnapshot, ctx: &Context) -> Vec<LintFinding> {
        lint::lint(note, ctx, &self.policy)
    }

    pub fn sweep(&self, ctx: &Context) -> SweepReport {
        sweep::sweep(ctx, &self.policy)
    }

    /// Conflicts between active claims.
    pub fn conflicts(&self, snapshot: &Snapshot) -> Vec<Conflict> {
        conflict::active_conflicts(snapshot, &self.policy)
    }

    pub fn propagate_conflicts(&self, ctx: &Context) -> ConflictPlan {
        gates::propagate_conflicts(ctx, &self.policy)
    }

    /// `wi_work`: a conflict work item for a pair without a slot.
    pub fn open_conflict_work(&self, a: &str, b: &str, reason: &str, now: Timestamp) -> WorkItem {
        gates::open_conflict_work(a, b, reason, now)
    }

    /// Supersede recovery after a crash between the candidate and target writes.
    pub fn recover_supersedes(&self, ctx: &Context) -> Vec<NoteEdit> {
        gates::recover_supersedes(ctx, &self.policy)
    }

    pub fn on_file_ingress(
        &self,
        previous: Option<&NoteSnapshot>,
        current: &NoteSnapshot,
        now: Timestamp,
    ) -> Option<NoteEdit> {
        gates::on_file_ingress(previous, current, now, &self.policy)
    }

    pub fn mcp_claim_edit(
        &self,
        old: &Note,
        old_path: &str,
        changes: &BTreeMap<String, serde_json::Value>,
        now: Timestamp,
    ) -> Result<Note, RevisionError> {
        revision::mcp_claim_edit(old, old_path, changes, now)
    }

    /// `mcp_claim_edit` for a note whose body edit is already applied.
    pub fn mcp_revision(
        &self,
        old: &Note,
        old_path: &str,
        changes: &BTreeMap<String, serde_json::Value>,
        now: Timestamp,
    ) -> Result<Note, RevisionError> {
        revision::mcp_revision(old, old_path, changes, now)
    }

    pub fn restore_revision(
        &self,
        old: &Note,
        old_path: &str,
        commit: Option<&str>,
        origin: OriginKind,
        now: Timestamp,
    ) -> Result<Note, RevisionError> {
        revision::restore_revision(old, old_path, commit, origin, now)
    }

    /// Capture and proposal filter. With `lifecycle: off` nothing is filtered.
    pub fn capture_filter(&self, text: &str, existing: &BTreeSet<String>) -> FilterOutcome {
        if !self.policy.lifecycle_on {
            return if text.trim().is_empty() {
                FilterOutcome::Empty
            } else {
                FilterOutcome::Accept
            };
        }
        capture::capture_filter(text, existing)
    }
}
