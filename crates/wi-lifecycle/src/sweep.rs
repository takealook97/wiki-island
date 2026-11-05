//! `wi sweep`: create or refresh work items. It returns no note edits, so it
//! cannot change any status.

use std::collections::BTreeMap;

use serde::Serialize;
use wi_note::{CaptureStatus, LifecycleType, Value, WorkState};

use crate::conflict::active_conflicts;
use crate::eligibility::Withheld;
use crate::freshness::{Freshness, FreshnessState, freshness};
use crate::model::{Context, NoteSnapshot, Policy};
use crate::time::format_time;
use crate::work::{Trigger, WorkItem, WorkKind};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SweepReport {
    /// New work items.
    pub create: Vec<WorkItem>,
    /// Items whose work note already exists (open, or closed with a matching
    /// reopen trigger), keyed by the existing note path.
    pub refresh: Vec<(String, WorkItem)>,
}

fn recheck(note: &NoteSnapshot, ctx: &Context, policy: &Policy) -> Option<WorkItem> {
    if !note.is_active() || policy.is_archived_path(&note.path) {
        return None;
    }
    let fresh = freshness(&note.lifecycle, &ctx.snapshot.evidence, policy, ctx.now);
    let (reason, action, trigger) = match &fresh.state {
        FreshnessState::Drifted(refs) => (
            format!("pinned source changed: {}", refs.join(", ")),
            "re-read the source; unchanged renews, changed creates a new candidate revision",
            Trigger::SourceChange,
        ),
        FreshnessState::SourceMissing(refs) | FreshnessState::NoSnapshot(refs) => (
            format!("pinned source missing: {}", refs.join(", ")),
            "restore the source or close as evidence_unavailable with a reopen trigger",
            Trigger::SourceChange,
        ),
        FreshnessState::Stale | FreshnessState::TtlExpired => (
            "claim aged out of its freshness window".to_string(),
            "recheck the source; a host fetch renews a URL snapshot",
            Trigger::Cooldown,
        ),
        FreshnessState::FutureTimestamp(field) => (
            format!("future timestamp in {field}"),
            "correct the timestamp",
            Trigger::Session,
        ),
        _ => return None,
    };
    let condition = recheck_condition(note, ctx, &fresh);
    Some(
        WorkItem::new(
            WorkKind::Recheck,
            &note.path,
            Vec::new(),
            reason,
            action,
            trigger,
            ctx.now,
        )
        .with_condition(condition),
    )
}

/// The observed state behind a recheck: freshness state, the current hash
/// (or absence) of every pinned source and `last_verified`.
fn recheck_condition(note: &NoteSnapshot, ctx: &Context, fresh: &Freshness) -> String {
    let sources: Vec<String> = note
        .lifecycle
        .sources
        .iter()
        .flatten()
        .map(|pin| {
            let hash = ctx
                .snapshot
                .evidence
                .get(&pin.reference)
                .and_then(|s| s.hash());
            format!("{}={}", pin.reference, hash.as_deref().unwrap_or("missing"))
        })
        .collect();
    let verified = fresh.last_verified.map(format_time).unwrap_or_default();
    format!(
        "{}; {}; last_verified={verified}",
        Withheld::Freshness(fresh.state.clone()).code(),
        sources.join(", ")
    )
}

fn pending_capture(note: &NoteSnapshot, ctx: &Context) -> Option<WorkItem> {
    let lc = &note.lifecycle;
    let pending = lc.lifecycle_type == LifecycleType::Capture
        && lc.capture_status != Some(CaptureStatus::Resolved);
    pending.then(|| {
        WorkItem::new(
            WorkKind::PendingCapture,
            &note.path,
            Vec::new(),
            "capture is pending",
            "list its outputs or give an exact no_memory_reason",
            Trigger::Session,
            ctx.now,
        )
        .with_condition(format!(
            "pending body={}",
            note.body_hash.as_deref().unwrap_or("empty")
        ))
    })
}

/// A closed item reopens only when its `reopen.trigger` matches and the
/// trigger condition is now observed in a state other than the one recorded
/// at closure. A closure without a recorded state is never reopened here.
fn reopens(reopen: Option<&Value>, item: &WorkItem) -> bool {
    let Some(reopen) = reopen else {
        return false;
    };
    let trigger = reopen
        .get("trigger")
        .and_then(|t| serde_json::from_value::<Trigger>(t.clone()).ok());
    let observed = reopen.get("observed").and_then(Value::as_str);
    trigger == Some(item.next_trigger)
        && observed.is_some_and(|o| item.condition.as_deref() != Some(o))
}

/// Whether the maintenance pass may close `item` when a sweep no longer
/// produces it: only items whose cause a sweep detects (pinned-source
/// rechecks, pending captures, slot conflicts, procedure health). Items
/// opened by hand or converted from device work requests stay open.
pub fn auto_closable(item: &WorkItem) -> bool {
    let Some(condition) = item.condition.as_deref() else {
        return false;
    };
    match item.kind {
        WorkKind::Recheck => condition.contains("; last_verified="),
        WorkKind::PendingCapture => condition.starts_with("pending body="),
        WorkKind::Conflict => condition.starts_with("slot "),
        WorkKind::Procedure => condition.starts_with("procedure: "),
        WorkKind::PromoteCandidate => condition.starts_with("promote-candidate: "),
        WorkKind::Duplicate => condition.starts_with("duplicate: "),
        _ => false,
    }
}

pub fn sweep(ctx: &Context, policy: &Policy) -> SweepReport {
    if !policy.lifecycle_on {
        return SweepReport::default();
    }
    let notes = ctx.snapshot.notes();
    let mut items: Vec<WorkItem> = notes
        .iter()
        .filter_map(|n| recheck(n, ctx, policy).or_else(|| pending_capture(n, ctx)))
        .collect();
    items.extend(active_conflicts(ctx.snapshot, policy).into_iter().map(|c| {
        let value = |path: &str| {
            ctx.snapshot
                .get(path)
                .and_then(|n| n.lifecycle.value.as_ref())
                .map(|v| v.to_string())
                .unwrap_or_default()
        };
        let condition = format!(
            "slot {}: {}={}, {}={}",
            c.slot,
            c.a,
            value(&c.a),
            c.b,
            value(&c.b)
        );
        crate::gates::open_conflict_work(
            &c.a,
            &c.b,
            &format!("active claims conflict on slot {}; neither wins", c.slot),
            ctx.now,
        )
        .with_condition(condition)
    }));
    let existing: BTreeMap<&str, &NoteSnapshot> = notes
        .iter()
        .filter(|n| n.lifecycle.lifecycle_type == LifecycleType::Work)
        .filter_map(|n| Some((n.work_id.as_deref()?, n)))
        .collect();
    items.sort_by(|a, b| a.id.cmp(&b.id));
    items.dedup_by(|a, b| a.id == b.id);
    let mut report = SweepReport::default();
    for item in items {
        let Some(note) = existing.get(item.id.as_str()) else {
            report.create.push(item);
            continue;
        };
        let work = note.lifecycle.work.as_ref();
        let closed = work.is_some_and(|w| w.state == Some(WorkState::Closed));
        if !closed || reopens(work.and_then(|w| w.reopen.as_ref()), &item) {
            report.refresh.push((note.path.clone(), item));
        }
    }
    report.create.sort_by(|a, b| a.id.cmp(&b.id));
    report.refresh.sort_by(|a, b| a.0.cmp(&b.0));
    report
}
