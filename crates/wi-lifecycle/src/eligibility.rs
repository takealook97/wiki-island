//! `current` read eligibility (KNOWLEDGE "Reads and delivery").

use std::collections::BTreeMap;

use serde::Serialize;
use wi_note::{ConditionOp, LifecycleType, Status};

use crate::attach::{deliverable_span, is_literal, missing_attachments};
use crate::conflict::{claims_conflict, competes, replaces, structurally_valid};
use crate::freshness::{FreshnessState, freshness, is_user_origin};
use crate::model::{Context, NoteSnapshot, Policy};
use crate::time::parse_time;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "reason", content = "detail")]
pub enum Withheld {
    Unparseable(String),
    NotActive(String),
    /// An active rule or preference whose origin is not `user` (defense in depth).
    OriginRule,
    Archived,
    Excluded,
    /// Listed in another active note's `supersedes`.
    Superseded(String),
    /// In conflict with another active claim.
    Conflict(Vec<String>),
    NotYetApplicable,
    ApplicabilityExpired,
    /// The note has a condition the query does not supply.
    UnknownCondition(String),
    ConditionUnmet(String),
    /// `applicability.conditions` is null: nobody has resolved them.
    UnresolvedConditions,
    Freshness(FreshnessState),
    /// A non-literal claim whose source spans cannot be delivered with it.
    AttachmentUnavailable(Vec<String>),
}

impl Withheld {
    /// Stable reason code for the bounded withheld list (counts and reasons, no content).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unparseable(_) => "unparseable",
            Self::NotActive(_) => "not-active",
            Self::OriginRule => "origin-rule",
            Self::Archived => "archived",
            Self::Excluded => "excluded",
            Self::Superseded(_) => "superseded",
            Self::Conflict(_) => "conflict",
            Self::NotYetApplicable => "not-yet-applicable",
            Self::ApplicabilityExpired => "applicability-expired",
            Self::UnknownCondition(_) => "unknown-condition",
            Self::ConditionUnmet(_) => "condition-unmet",
            Self::UnresolvedConditions => "unresolved-conditions",
            Self::AttachmentUnavailable(_) => "attachment-unavailable",
            Self::Freshness(state) => match state {
                FreshnessState::Fresh | FreshnessState::Timeless => "fresh",
                FreshnessState::Stale => "stale",
                FreshnessState::TtlExpired => "ttl-expired",
                FreshnessState::Drifted(_) => "drift",
                FreshnessState::SourceMissing(_) => "source-missing",
                FreshnessState::NoSnapshot(_) => "no-snapshot",
                FreshnessState::FutureTimestamp(_) => "future-timestamp",
                FreshnessState::MissingObservedAt => "missing-observed-at",
                FreshnessState::MissingEvidenceTime(_) => "missing-evidence-time",
                FreshnessState::Unparseable(_) => "unparseable-time",
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "eligibility")]
pub enum Eligibility {
    Current,
    Withheld(Withheld),
    /// Not a memory claim: plain (`lifecycle: none`), capture or work notes,
    /// and every note when `lifecycle: off`. Served as plain content.
    Plain {
        lifecycle: LifecycleType,
    },
}

impl Eligibility {
    pub fn is_current(&self) -> bool {
        *self == Self::Current
    }
}

fn status_text(status: &Option<Status>) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "null".into())
}

/// Another active note that validly names `note` in its `supersedes`.
fn superseded_by<'a>(note: &NoteSnapshot, ctx: &'a Context, policy: &Policy) -> Option<&'a str> {
    ctx.snapshot
        .notes()
        .iter()
        .filter(|other| competes(other, policy) && other.path != note.path)
        .filter(|other| other.supersedes().iter().any(|name| note.is_named(name)))
        .find(|other| structurally_valid(other, note).is_ok())
        .map(|other| other.path.as_str())
}

fn conflicting_partners(note: &NoteSnapshot, ctx: &Context, policy: &Policy) -> Vec<String> {
    let partner = |other: &&NoteSnapshot| {
        competes(other, policy) && other.path != note.path && !replaces(note, other)
    };
    let flagged = note
        .lifecycle
        .conflict_with
        .iter()
        .flatten()
        .filter_map(|name| ctx.snapshot.resolve(name))
        .filter(partner)
        .map(|other| other.path.clone());
    let detected = ctx
        .snapshot
        .notes()
        .iter()
        .filter(partner)
        .filter(|other| claims_conflict(note, other).is_some())
        .map(|other| other.path.clone());
    let mut partners: Vec<String> = flagged.chain(detected).collect();
    partners.sort();
    partners.dedup();
    partners
}

fn applicability(note: &NoteSnapshot, ctx: &Context) -> Option<Withheld> {
    let app = note.lifecycle.applicability.as_ref()?;
    let parse = |t: &Option<String>| t.as_deref().map(parse_time).transpose();
    match (parse(&app.from), parse(&app.until)) {
        (Ok(from), Ok(until)) => {
            if from.is_some_and(|f| ctx.now < f) {
                return Some(Withheld::NotYetApplicable);
            }
            if until.is_some_and(|u| ctx.now >= u) {
                return Some(Withheld::ApplicabilityExpired);
            }
        }
        (Err(e), _) | (_, Err(e)) => return Some(Withheld::Unparseable(e.to_string())),
    }
    // A user note's absent conditions read as `[]`, the
    // same rule the classification gate applies; an explicit null stays unresolved.
    let Some(conditions) = &app.conditions else {
        let exempt = is_user_origin(&note.lifecycle) && note.conditions_absent;
        return (!exempt).then_some(Withheld::UnresolvedConditions);
    };
    conditions.iter().find_map(|condition| {
        let Some(given) = ctx.query.conditions.get(&condition.key) else {
            return Some(Withheld::UnknownCondition(condition.key.clone()));
        };
        let met = match condition.op {
            ConditionOp::Eq => condition.values.first() == Some(given),
            ConditionOp::In => condition.values.contains(given),
        };
        (!met).then(|| Withheld::ConditionUnmet(condition.key.clone()))
    })
}

pub fn eligibility(note: &NoteSnapshot, ctx: &Context, policy: &Policy) -> Eligibility {
    if !policy.lifecycle_on {
        return Eligibility::Plain {
            lifecycle: LifecycleType::None,
        };
    }
    if let Some(reason) = &note.unparseable {
        // Malformed frontmatter cannot declare a lifecycle type: serve it as plain text.
        if note.lifecycle.lifecycle_type == LifecycleType::None && note.claim_content.is_none() {
            return Eligibility::Plain {
                lifecycle: LifecycleType::None,
            };
        }
        return Eligibility::Withheld(Withheld::Unparseable(reason.clone()));
    }
    if !note.is_memory() {
        return Eligibility::Plain {
            lifecycle: note.lifecycle.lifecycle_type,
        };
    }
    let withheld = |w| Eligibility::Withheld(w);
    if note.lifecycle.status != Some(Status::Active) {
        return withheld(Withheld::NotActive(status_text(&note.lifecycle.status)));
    }
    let user_origin = note
        .lifecycle
        .origin
        .as_ref()
        .is_some_and(|o| o.kind == wi_note::OriginKind::User);
    if note.is_rule_like() && !user_origin {
        return withheld(Withheld::OriginRule);
    }
    if policy.is_archived_path(&note.path) {
        return withheld(Withheld::Archived);
    }
    if note.lifecycle.excluded == Some(true) {
        return withheld(Withheld::Excluded);
    }
    if let Some(by) = superseded_by(note, ctx, policy) {
        return withheld(Withheld::Superseded(by.to_string()));
    }
    let partners = conflicting_partners(note, ctx, policy);
    if !partners.is_empty() {
        return withheld(Withheld::Conflict(partners));
    }
    if let Some(reason) = applicability(note, ctx) {
        return withheld(reason);
    }
    let fresh = freshness(&note.lifecycle, &ctx.snapshot.evidence, policy, ctx.now);
    if !fresh.citable() {
        return withheld(Withheld::Freshness(fresh.state));
    }
    if !delivers_literal(note, ctx) {
        let missing = missing_attachments(&note.lifecycle, &ctx.snapshot.evidence);
        if !missing.is_empty() {
            return withheld(Withheld::AttachmentUnavailable(missing));
        }
    }
    Eligibility::Current
}

/// Recorded `adoption.literal: true` that the current evidence still bears out.
fn delivers_literal(note: &NoteSnapshot, ctx: &Context) -> bool {
    let recorded = note
        .lifecycle
        .adoption
        .as_ref()
        .and_then(|a| a.literal)
        .unwrap_or(false);
    recorded && is_literal(&note.lifecycle, &ctx.snapshot.evidence)
}

/// The source span delivered with a non-literal claim, so qualifiers travel with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttachedSpan {
    pub reference: String,
    pub text: String,
    /// URL snapshot observers of the pinned bytes; empty for local sources.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub observed_by: Vec<String>,
}

/// Spans delivered with a non-literal claim. `eligibility` withholds a claim
/// whose required spans cannot be delivered, so a current claim's list is complete.
pub fn attached_spans(note: &NoteSnapshot, ctx: &Context) -> Vec<AttachedSpan> {
    if delivers_literal(note, ctx) {
        return Vec::new();
    }
    let evidence = &ctx.snapshot.evidence;
    note.lifecycle
        .sources
        .iter()
        .flatten()
        .filter_map(|pin| {
            let text = deliverable_span(pin, evidence).ok()?;
            Some(AttachedSpan {
                reference: pin.reference.clone(),
                text,
                observed_by: if crate::freshness::is_url_pin(pin) {
                    evidence
                        .get(&pin.reference)
                        .map(|s| s.observers(&pin.source_hash))
                        .unwrap_or_else(|| vec!["unknown".into()])
                } else {
                    Vec::new()
                },
            })
        })
        .collect()
}

/// A `current` read over the whole snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReadPartition {
    pub current: Vec<String>,
    pub plain: Vec<String>,
    /// Counts per reason code; never content.
    pub withheld: BTreeMap<String, usize>,
}

pub fn partition_current(ctx: &Context, policy: &Policy) -> ReadPartition {
    let mut partition = ReadPartition::default();
    for note in ctx.snapshot.notes() {
        match eligibility(note, ctx, policy) {
            Eligibility::Current => partition.current.push(note.path.clone()),
            Eligibility::Plain { .. } => partition.plain.push(note.path.clone()),
            Eligibility::Withheld(reason) => {
                *partition.withheld.entry(reason.code().into()).or_insert(0) += 1;
            }
        }
    }
    partition
}
