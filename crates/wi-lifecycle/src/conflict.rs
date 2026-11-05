//! Model-free slot conflicts and `supersedes` target validation.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use wi_note::{Applicability, ClaimKind, ConditionOp, Value};

use crate::freshness::{last_verified, missing_evidence_times};
use crate::model::{Evidence, NoteSnapshot, Policy, Snapshot};
use crate::time::{Timestamp, parse_time};

/// Two claims that share a slot, an entity and a scope, overlap in
/// applicability and differ in value. Neither wins.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Conflict {
    /// The lexically smaller path.
    pub a: String,
    pub b: String,
    pub slot: String,
    pub entities: Vec<String>,
}

fn bound(text: Option<&str>) -> Option<Option<Timestamp>> {
    match text {
        None => Some(None),
        Some(t) => parse_time(t).ok().map(Some),
    }
}

/// Half-open interval overlap; unparseable bounds count as overlapping.
fn windows_overlap(a: &Applicability, b: &Applicability) -> bool {
    let (Some(af), Some(au), Some(bf), Some(bu)) = (
        bound(a.from.as_deref()),
        bound(a.until.as_deref()),
        bound(b.from.as_deref()),
        bound(b.until.as_deref()),
    ) else {
        return true;
    };
    let starts_before_end = |from: Option<Timestamp>, until: Option<Timestamp>| match (from, until)
    {
        (Some(f), Some(u)) => f < u,
        _ => true,
    };
    starts_before_end(af, bu) && starts_before_end(bf, au)
}

fn condition_values(app: &Applicability, key: &str) -> Option<Vec<Value>> {
    app.conditions
        .as_ref()?
        .iter()
        .find(|c| c.key == key)
        .map(|c| match c.op {
            ConditionOp::Eq => c.values.iter().take(1).cloned().collect(),
            ConditionOp::In => c.values.clone(),
        })
}

/// Conditions are conjunctions, so a shared key with disjoint values never overlaps.
fn conditions_overlap(a: &Applicability, b: &Applicability) -> bool {
    a.conditions.iter().flatten().all(|condition| {
        let Some(other) = condition_values(b, &condition.key) else {
            return true;
        };
        let mine = condition_values(a, &condition.key).unwrap_or_default();
        mine.iter().any(|v| other.contains(v))
    })
}

pub fn applicability_overlaps(a: Option<&Applicability>, b: Option<&Applicability>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => windows_overlap(a, b) && conditions_overlap(a, b),
        _ => true,
    }
}

fn shared_entities(a: &NoteSnapshot, b: &NoteSnapshot) -> Vec<String> {
    let theirs: BTreeSet<String> = b.entity_ids().into_iter().collect();
    a.entity_ids()
        .into_iter()
        .filter(|id| theirs.contains(id))
        .collect()
}

/// The slot conflict predicate. Without a slot there is no automatic detection.
pub fn claims_conflict(a: &NoteSnapshot, b: &NoteSnapshot) -> Option<Conflict> {
    let (la, lb) = (&a.lifecycle, &b.lifecycle);
    let slot = la.slot.as_ref().filter(|s| lb.slot.as_ref() == Some(*s))?;
    let scope = la.scope.as_ref()?;
    if lb.scope.as_ref() != Some(scope) || a.path == b.path {
        return None;
    }
    let entities = shared_entities(a, b);
    let (va, vb) = (la.value.as_ref()?, lb.value.as_ref()?);
    let overlap = applicability_overlaps(la.applicability.as_ref(), lb.applicability.as_ref());
    if entities.is_empty() || va == vb || !overlap {
        return None;
    }
    let (first, second) = if a.path <= b.path { (a, b) } else { (b, a) };
    Some(Conflict {
        a: first.path.clone(),
        b: second.path.clone(),
        slot: slot.clone(),
        entities,
    })
}

/// Whether a note takes part in active-claim conflict detection.
pub(crate) fn competes(note: &NoteSnapshot, policy: &Policy) -> bool {
    note.is_active() && note.unparseable.is_none() && !policy.is_archived_path(&note.path)
}

/// Every conflict between two active claims, sorted.
pub fn active_conflicts(snapshot: &Snapshot, policy: &Policy) -> Vec<Conflict> {
    if !policy.lifecycle_on {
        return Vec::new();
    }
    // Only claims that share a slot can conflict: compare within each slot
    // (path order kept), not every pair of the vault.
    let mut by_slot: BTreeMap<&str, Vec<&NoteSnapshot>> = BTreeMap::new();
    for note in snapshot.notes().iter().filter(|n| competes(n, policy)) {
        if let Some(slot) = note.lifecycle.slot.as_deref() {
            by_slot.entry(slot).or_default().push(note);
        }
    }
    let mut found: Vec<Conflict> = by_slot
        .values()
        .flat_map(|active| {
            active.iter().enumerate().flat_map(move |(i, a)| {
                active[i + 1..]
                    .iter()
                    .filter(move |b| !replaces(a, b))
                    .filter_map(move |b| claims_conflict(a, b))
            })
        })
        .collect();
    found.sort();
    found
}

/// Whether one note validly names the other in `supersedes`: such a
/// pair is a replacement in progress, not a conflict.
pub(crate) fn replaces(a: &NoteSnapshot, b: &NoteSnapshot) -> bool {
    let names = |x: &NoteSnapshot, y: &NoteSnapshot| {
        x.supersedes().iter().any(|n| y.is_named(n)) && structurally_valid(x, y).is_ok()
    };
    names(a, b) || names(b, a)
}

/// Why a `supersedes` target does not earn the conflict exemption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InvalidTarget {
    Unresolved,
    SelfReference,
    /// A rule or preference loses visibility only through user (file) ingress.
    RuleRetirement,
    KindMismatch,
    ScopeMismatch,
    NoSharedEntity,
    SlotMismatch,
    /// The candidate's `last_verified` is earlier than the current-state target's.
    OlderEvidence,
    /// A non-user candidate pins a source without an engine-recorded evidence time.
    UnknownEvidenceTime,
}

impl InvalidTarget {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unresolved => "unresolved",
            Self::UnknownEvidenceTime => "unknown-evidence-time",
            Self::SelfReference => "self-reference",
            Self::RuleRetirement => "rule-retirement",
            Self::KindMismatch => "kind-mismatch",
            Self::ScopeMismatch => "scope-mismatch",
            Self::NoSharedEntity => "no-shared-entity",
            Self::SlotMismatch => "slot-mismatch",
            Self::OlderEvidence => "older-evidence",
        }
    }
}

/// Structural validity: what a read can still check after adoption.
pub(crate) fn structurally_valid(
    candidate: &NoteSnapshot,
    target: &NoteSnapshot,
) -> Result<(), InvalidTarget> {
    let (c, t) = (&candidate.lifecycle, &target.lifecycle);
    if candidate.path == target.path {
        return Err(InvalidTarget::SelfReference);
    }
    let user_origin = c
        .origin
        .as_ref()
        .is_some_and(|o| o.kind == wi_note::OriginKind::User);
    if target.is_rule_like() && !user_origin {
        return Err(InvalidTarget::RuleRetirement);
    }
    if c.kind.is_none() || c.kind != t.kind {
        return Err(InvalidTarget::KindMismatch);
    }
    if c.scope.is_none() || c.scope != t.scope {
        return Err(InvalidTarget::ScopeMismatch);
    }
    if shared_entities(candidate, target).is_empty() {
        return Err(InvalidTarget::NoSharedEntity);
    }
    if t.slot.is_some() && c.slot != t.slot {
        return Err(InvalidTarget::SlotMismatch);
    }
    Ok(())
}

/// The conflict exemption applies only to a valid target.
pub fn validate_supersedes(
    candidate: &NoteSnapshot,
    target: &NoteSnapshot,
    evidence: &Evidence,
    now: Timestamp,
) -> Result<(), InvalidTarget> {
    structurally_valid(candidate, target)?;
    if !missing_evidence_times(&candidate.lifecycle, evidence).is_empty() {
        return Err(InvalidTarget::UnknownEvidenceTime);
    }
    if target.lifecycle.claim_kind == Some(ClaimKind::CurrentState) {
        let mine = last_verified(&candidate.lifecycle, evidence, now)
            .ok()
            .flatten();
        let theirs = last_verified(&target.lifecycle, evidence, now)
            .ok()
            .flatten();
        match (mine, theirs) {
            (Some(m), Some(t)) if m >= t => {}
            (Some(_), None) => {}
            _ => return Err(InvalidTarget::OlderEvidence),
        }
    }
    Ok(())
}
