//! Adoption gates and the edits that follow from them.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};
use wi_note::{ClaimKind, EditError, Kind, Location, Note, OriginKind, Status, set_key};

use crate::attach::{is_literal, missing_attachments, span_text};
use crate::budget::{ActiveCounts, BudgetExhausted, LossEvent, LossKind, LossRequest};
use crate::complete::complete_user_defaults;
use crate::conflict::{
    InvalidTarget, active_conflicts, claims_conflict, competes, validate_supersedes,
};
use crate::freshness::{is_user_origin, last_verified};
use crate::lint::lint;
use crate::model::{Actor, Context, NoteSnapshot, Policy, sha256};
use crate::queue::{DeferredAction, DeferredEntry};
use crate::time::format_time;
use crate::work::{Trigger, WorkItem, WorkKind};

pub const GATES: &[&str] = &[
    "sources",
    "classification",
    "literal",
    "lint",
    "supersedes",
    "conflict",
    "origin",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum GateResult {
    Pass,
    Fail(String),
}

impl GateResult {
    /// Frontmatter form: `pass` or `fail: <reason>`.
    pub fn to_value(&self) -> Value {
        match self {
            Self::Pass => json!("pass"),
            Self::Fail(reason) => json!(format!("fail: {reason}")),
        }
    }

    pub fn passed(&self) -> bool {
        *self == Self::Pass
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct GateReport {
    pub results: BTreeMap<String, GateResult>,
    pub literal: bool,
}

impl GateReport {
    pub fn failed(&self) -> Vec<&str> {
        self.results
            .iter()
            .filter(|(_, r)| !r.passed())
            .map(|(k, _)| k.as_str())
            .collect()
    }

    fn gates_value(&self) -> Value {
        Value::Object(
            self.results
                .iter()
                .map(|(k, r)| (k.clone(), r.to_value()))
                .collect(),
        )
    }
}

/// Top-level frontmatter keys to set on one note, applied byte-preservingly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NoteEdit {
    pub path: String,
    pub set: BTreeMap<String, Value>,
}

impl NoteEdit {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            set: BTreeMap::new(),
        }
    }

    pub fn with(self, key: &str, value: Value) -> Self {
        let mut set = self.set;
        set.insert(key.to_string(), value);
        Self {
            path: self.path,
            set,
        }
    }

    /// Apply with `wi_note::set_key`, key by key; returns a new note.
    pub fn apply(&self, note: &Note) -> Result<Note, EditError> {
        self.set
            .iter()
            .try_fold(note.clone(), |note, (key, value)| {
                set_key(&note, key, value)
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "reason", content = "detail")]
pub enum RejectReason {
    /// A rule or preference whose origin is not `user`.
    OriginRule,
    /// Equal body hash to this active claim.
    Duplicate(String),
    Unparseable(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "reason", content = "detail")]
pub enum UnchangedReason {
    LifecycleOff,
    NotMemory,
    /// `superseded` and `rejected` are terminal and never revived.
    Terminal(String),
    /// Only candidates are evaluated.
    NotCandidate(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "outcome")]
pub enum AdoptionOutcome {
    /// Every gate passed. Apply `edits` in order: the candidate first, then targets.
    Active {
        edits: Vec<NoteEdit>,
        gates: GateReport,
        /// Loss events to record durably (manifest record and commit trailer).
        losses: Vec<LossEvent>,
    },
    Candidate {
        edits: Vec<NoteEdit>,
        gates: GateReport,
        work: Vec<WorkItem>,
    },
    /// Gates passed but the change cap is spent: queued with a work item.
    Deferred {
        edits: Vec<NoteEdit>,
        gates: GateReport,
        exhausted: BudgetExhausted,
        entry: DeferredEntry,
        work: Vec<WorkItem>,
    },
    Rejected {
        reason: RejectReason,
        edits: Vec<NoteEdit>,
        work: Vec<WorkItem>,
    },
    Unchanged {
        reason: UnchangedReason,
    },
}

fn status_text(status: &Status) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn origin_kind(note: &NoteSnapshot) -> Option<OriginKind> {
    note.lifecycle.origin.as_ref().map(|o| o.kind.clone())
}

fn actor_of(note: &NoteSnapshot) -> Actor {
    match origin_kind(note) {
        Some(OriginKind::User) => Actor::User,
        _ => Actor::Agent,
    }
}

/// The `adoption` block written into the note. Kept small for note apps:
/// `gates` only when a gate failed (it says what to fix) and `literal` only
/// when true; absent keys read as "all passed" and "not literal".
fn adoption_value(ctx: &Context, gates: &GateReport, adopted: bool) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("at".into(), json!(adopted.then(|| format_time(ctx.now))));
    map.insert("by".into(), json!(ctx.by));
    if !gates.failed().is_empty() {
        map.insert("gates".into(), gates.gates_value());
    }
    if gates.literal {
        map.insert("literal".into(), json!(true));
    }
    Value::Object(map)
}

fn sources_gate(note: &NoteSnapshot, ctx: &Context) -> GateResult {
    let lc = &note.lifecycle;
    // A user-origin claim is self-attested; the owner is
    // the evidence, so missing or empty pins pass. Present pins are verified.
    let user = is_user_origin(lc);
    let needs = !user
        && (matches!(lc.kind, Some(Kind::Fact) | Some(Kind::Decision))
            || lc.claim_kind == Some(ClaimKind::CurrentState));
    let pins = match lc.sources.as_ref() {
        Some(pins) => pins,
        None if user => return GateResult::Pass,
        None => return GateResult::Fail("sources unresolved".into()),
    };
    if needs && pins.is_empty() {
        return GateResult::Fail("no source pin".into());
    }
    // A non-user claim's time is lowered to its pins' engine-recorded
    // evidence times, so an unknown time cannot be trusted (fail closed).
    let needs_time = !user;
    let problems: Vec<String> = pins
        .iter()
        .filter_map(|pin| {
            let state = ctx.snapshot.evidence.get(&pin.reference);
            let Some(bytes) = state.and_then(|s| s.bytes.as_deref()) else {
                return Some(format!("missing {}", pin.reference));
            };
            if sha256(bytes) != pin.source_hash {
                return Some(format!("hash mismatch {}", pin.reference));
            }
            if needs_time && state.and_then(|s| s.evidence_time).is_none() {
                return Some(format!("no engine evidence time {}", pin.reference));
            }
            let span = pin.locator.as_ref().and_then(|l| match &l.location {
                Location::Span(span) => Some(span),
                _ => None,
            })?;
            span_text(bytes, span.start, span.end, pin.span_hash.as_deref())
                .err()
                .map(|problem| format!("{} {}", problem.code(), pin.reference))
        })
        .collect();
    if problems.is_empty() {
        GateResult::Pass
    } else {
        GateResult::Fail(problems.join(", "))
    }
}

fn classification_gate(note: &NoteSnapshot) -> GateResult {
    let lc = &note.lifecycle;
    let summary_ok = lc.summary.as_ref().is_some_and(|s| !s.trim().is_empty());
    // A user note's absent `applicability.conditions` key
    // reads as `[]`; an explicit null stays unresolved. The engine never
    // inserts into an existing mapping.
    let user = is_user_origin(lc) && note.conditions_absent;
    let missing: Vec<&str> = [
        ("kind", lc.kind.is_some()),
        ("claim_kind", lc.claim_kind.is_some()),
        ("summary", summary_ok),
        ("applicability", lc.applicability.is_some()),
        (
            "applicability.conditions",
            lc.applicability
                .as_ref()
                .is_none_or(|a| user || a.conditions.is_some()),
        ),
        ("entities", lc.entities.is_some()),
        ("scope", lc.scope.is_some()),
    ]
    .into_iter()
    .filter(|(_, ok)| !ok)
    .map(|(k, _)| k)
    .collect();
    if missing.is_empty() {
        GateResult::Pass
    } else {
        GateResult::Fail(format!("null {}", missing.join(", ")))
    }
}

/// Gate 3: a non-literal claim must be able to deliver its source spans.
fn literal_gate(note: &NoteSnapshot, ctx: &Context, literal: bool) -> GateResult {
    if literal {
        return GateResult::Pass;
    }
    let missing = missing_attachments(&note.lifecycle, &ctx.snapshot.evidence);
    if missing.is_empty() {
        GateResult::Pass
    } else {
        GateResult::Fail(format!("attachment-unavailable: {}", missing.join(", ")))
    }
}

fn lint_gate(note: &NoteSnapshot, ctx: &Context, policy: &Policy) -> GateResult {
    let codes: Vec<String> = lint(note, ctx, policy)
        .into_iter()
        .filter(|f| !f.code.covered_by_other_gate() && !f.code.informational())
        .map(|f| {
            serde_json::to_value(f.code)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        })
        .collect();
    if codes.is_empty() {
        GateResult::Pass
    } else {
        GateResult::Fail(codes.join(", "))
    }
}

/// Valid active targets, invalid targets with reasons.
type TargetSplit<'a> = (Vec<&'a NoteSnapshot>, Vec<(String, InvalidTarget)>);

fn split_targets<'a>(note: &NoteSnapshot, ctx: &'a Context, policy: &Policy) -> TargetSplit<'a> {
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    for name in note.supersedes() {
        let Some(target) = ctx.snapshot.resolve(&name) else {
            invalid.push((name, InvalidTarget::Unresolved));
            continue;
        };
        if !competes(target, policy) {
            continue;
        }
        match validate_supersedes(note, target, &ctx.snapshot.evidence, ctx.now) {
            Ok(()) => valid.push(target),
            Err(reason) => invalid.push((target.path.clone(), reason)),
        }
    }
    valid.sort_by(|a, b| a.path.cmp(&b.path));
    valid.dedup_by(|a, b| a.path == b.path);
    (valid, invalid)
}

fn duplicate_of<'a>(note: &NoteSnapshot, ctx: &'a Context, policy: &Policy) -> Option<&'a str> {
    let hash = note.body_hash.as_ref()?;
    let targets = note.supersedes();
    ctx.snapshot
        .notes()
        .iter()
        .filter(|o| competes(o, policy) && o.path != note.path)
        .filter(|o| !targets.iter().any(|t| o.is_named(t)))
        .find(|o| o.body_hash.as_ref() == Some(hash))
        .map(|o| o.path.as_str())
}

fn rejected(
    note: &NoteSnapshot,
    ctx: &Context,
    reason: RejectReason,
    work: Vec<WorkItem>,
) -> AdoptionOutcome {
    let gate = match &reason {
        RejectReason::OriginRule => ("origin", "origin.kind is not user"),
        RejectReason::Duplicate(_) => ("duplicate", "body hash equals an active claim"),
        RejectReason::Unparseable(_) => ("parse", "unparseable"),
    };
    let gates = GateReport {
        results: [(gate.0.to_string(), GateResult::Fail(gate.1.into()))].into(),
        literal: false,
    };
    let edits = if matches!(reason, RejectReason::Unparseable(_)) {
        Vec::new()
    } else {
        vec![
            NoteEdit::new(&note.path)
                .with("status", json!("rejected"))
                .with("adoption", adoption_value(ctx, &gates, false)),
        ]
    };
    AdoptionOutcome::Rejected {
        reason,
        edits,
        work,
    }
}

fn precheck(note: &NoteSnapshot, ctx: &Context, policy: &Policy) -> Option<AdoptionOutcome> {
    let unchanged = |reason| Some(AdoptionOutcome::Unchanged { reason });
    if !policy.lifecycle_on {
        return unchanged(UnchangedReason::LifecycleOff);
    }
    if let Some(reason) = &note.unparseable {
        return Some(rejected(
            note,
            ctx,
            RejectReason::Unparseable(reason.clone()),
            Vec::new(),
        ));
    }
    if !note.is_memory() {
        return unchanged(UnchangedReason::NotMemory);
    }
    match &note.lifecycle.status {
        None | Some(Status::Candidate) => None,
        Some(s @ (Status::Superseded | Status::Rejected)) => {
            unchanged(UnchangedReason::Terminal(status_text(s)))
        }
        Some(s) => unchanged(UnchangedReason::NotCandidate(status_text(s))),
    }
}

/// Evaluate one candidate. Pure: the host applies `edits`, writes `work`
/// notes and records `losses` under the publisher lock.
pub fn evaluate_adoption(note: &NoteSnapshot, ctx: &Context, policy: &Policy) -> AdoptionOutcome {
    if let Some(outcome) = precheck(note, ctx, policy) {
        return outcome;
    }
    // Gates see the completed snapshot; the filled keys
    // ride in the same checksum-guarded edit that writes status and adoption.
    let (completed, filled) = complete_user_defaults(note);
    let outcome = evaluate_completed(&completed, ctx, policy);
    with_completion(outcome, &note.path, &filled)
}

/// Add the filled defaults to every edit of `path` and list their keys in
/// `adoption.completed`. Rejected and unchanged outcomes write no defaults.
fn with_completion(
    outcome: AdoptionOutcome,
    path: &str,
    filled: &[(&'static str, Value)],
) -> AdoptionOutcome {
    if filled.is_empty() {
        return outcome;
    }
    let complete = |edits: Vec<NoteEdit>| -> Vec<NoteEdit> {
        edits
            .into_iter()
            .map(|edit| {
                if edit.path != path {
                    return edit;
                }
                let keys: Vec<&str> = filled.iter().map(|(k, _)| *k).collect();
                let adoption = edit.set.get("adoption").cloned().map(|a| match a {
                    Value::Object(map) => {
                        let mut map = map;
                        map.insert("completed".into(), json!(keys));
                        Value::Object(map)
                    }
                    other => other,
                });
                let edit = filled.iter().fold(edit, |e, (k, v)| e.with(k, v.clone()));
                match adoption {
                    Some(a) => edit.with("adoption", a),
                    None => edit,
                }
            })
            .collect()
    };
    match outcome {
        AdoptionOutcome::Active {
            edits,
            gates,
            losses,
        } => AdoptionOutcome::Active {
            edits: complete(edits),
            gates,
            losses,
        },
        AdoptionOutcome::Candidate { edits, gates, work } => AdoptionOutcome::Candidate {
            edits: complete(edits),
            gates,
            work,
        },
        AdoptionOutcome::Deferred {
            edits,
            gates,
            exhausted,
            entry,
            work,
        } => AdoptionOutcome::Deferred {
            edits: complete(edits),
            gates,
            exhausted,
            entry,
            work,
        },
        other => other,
    }
}

fn evaluate_completed(note: &NoteSnapshot, ctx: &Context, policy: &Policy) -> AdoptionOutcome {
    if note.is_rule_like() && origin_kind(note) != Some(OriginKind::User) {
        let work = WorkItem::new(
            WorkKind::OriginRule,
            &note.path,
            note.supersedes(),
            "an agent proposed a rule or preference; it can never adopt from this origin",
            "edit the file directly to adopt the rule as the user",
            Trigger::Session,
            ctx.now,
        );
        return rejected(note, ctx, RejectReason::OriginRule, vec![work]);
    }
    if let Some(of) = duplicate_of(note, ctx, policy) {
        return rejected(
            note,
            ctx,
            RejectReason::Duplicate(of.to_string()),
            Vec::new(),
        );
    }
    let (valid, invalid) = split_targets(note, ctx, policy);
    let conflicts: Vec<String> = ctx
        .snapshot
        .notes()
        .iter()
        .filter(|o| competes(o, policy) && !valid.iter().any(|v| v.path == o.path))
        .filter(|o| claims_conflict(note, o).is_some())
        .map(|o| o.path.clone())
        .collect();
    let literal = is_literal(&note.lifecycle, &ctx.snapshot.evidence);
    let mut results = BTreeMap::new();
    results.insert("sources".into(), sources_gate(note, ctx));
    results.insert("classification".into(), classification_gate(note));
    results.insert("literal".into(), literal_gate(note, ctx, literal));
    results.insert("lint".into(), lint_gate(note, ctx, policy));
    results.insert("origin".into(), GateResult::Pass);
    let supersedes = if invalid.is_empty() {
        GateResult::Pass
    } else {
        let list: Vec<String> = invalid
            .iter()
            .map(|(p, r)| format!("{p} {}", r.code()))
            .collect();
        GateResult::Fail(format!("invalid target {}", list.join(", ")))
    };
    results.insert("supersedes".into(), supersedes);
    let conflict = if conflicts.is_empty() {
        GateResult::Pass
    } else {
        GateResult::Fail(format!("conflicts with {}", conflicts.join(", ")))
    };
    results.insert("conflict".into(), conflict);
    let gates = GateReport { results, literal };
    if !gates.failed().is_empty() {
        return candidate_outcome(note, ctx, gates, &invalid, &conflicts);
    }
    adopt(note, ctx, policy, gates, &valid)
}

fn candidate_outcome(
    note: &NoteSnapshot,
    ctx: &Context,
    gates: GateReport,
    invalid: &[(String, InvalidTarget)],
    conflicts: &[String],
) -> AdoptionOutcome {
    let mut edit = NoteEdit::new(&note.path)
        .with("status", json!("candidate"))
        .with("adoption", adoption_value(ctx, &gates, false));
    if !conflicts.is_empty() {
        let mut flagged: Vec<String> = note.lifecycle.conflict_with.clone().unwrap_or_default();
        flagged.extend(conflicts.iter().cloned());
        flagged.sort();
        flagged.dedup();
        edit = edit.with("conflict_with", json!(flagged));
    }
    let failed = gates.failed().join(", ");
    let mut work = vec![WorkItem::new(
        WorkKind::Gate,
        &note.path,
        Vec::new(),
        format!("gates failed: {failed}"),
        gate_guidance(&gates),
        Trigger::SourceChange,
        ctx.now,
    )];
    if !conflicts.is_empty() {
        work.push(WorkItem::new(
            WorkKind::Conflict,
            &note.path,
            conflicts.to_vec(),
            "candidate conflicts with an active claim; the active claim stays current",
            "decide which claim holds; no claim wins by time or vote",
            Trigger::SourceChange,
            ctx.now,
        ));
    }
    if !invalid.is_empty() {
        work.push(WorkItem::new(
            WorkKind::InvalidSupersedes,
            &note.path,
            invalid.iter().map(|(p, _)| p.clone()).collect(),
            "a supersedes target is invalid; the target stays current",
            "fix the supersedes list or supply newer evidence",
            Trigger::SourceChange,
            ctx.now,
        ));
    }
    AdoptionOutcome::Candidate {
        edits: vec![edit],
        gates,
        work,
    }
}

/// One sentence per failed gate saying what the engine still needs.
/// Gate result strings in `adoption.gates` are unchanged.
pub fn gate_guidance(gates: &GateReport) -> String {
    let sentences: Vec<String> = gates
        .results
        .iter()
        .filter_map(|(gate, result)| match result {
            GateResult::Pass => None,
            GateResult::Fail(reason) => Some(gate_sentence(gate, reason)),
        })
        .collect();
    if sentences.is_empty() {
        "rerun the gates".into()
    } else {
        sentences.join(" ")
    }
}

fn gate_sentence(gate: &str, reason: &str) -> String {
    match gate {
        "classification" => format!(
            "classification: add {} to the frontmatter.",
            reason.strip_prefix("null ").unwrap_or(reason)
        ),
        "sources" => match reason {
            "sources unresolved" => "sources: add a `sources` list of pins.".into(),
            "no source pin" => "sources: pin at least one source for this claim kind.".into(),
            pins => format!("sources: fix or re-pin the failing pins ({pins})."),
        },
        "conflict" => format!(
            "conflict: decide against {}; edit, supersede or archive one claim.",
            reason.strip_prefix("conflicts with ").unwrap_or(reason)
        ),
        "lint" => format!("lint: fix the reported codes ({reason}); see `wi lint`."),
        "origin" => {
            "origin: only the user can adopt a rule or preference; edit the file directly.".into()
        }
        "supersedes" => format!(
            "supersedes: fix the supersedes list or supply newer evidence ({}).",
            reason.strip_prefix("invalid target ").unwrap_or(reason)
        ),
        "literal" => format!(
            "literal: make the pinned source spans deliverable or quote the source span as the summary ({}).",
            reason
                .strip_prefix("attachment-unavailable: ")
                .unwrap_or(reason)
        ),
        other => format!("{other}: {reason}."),
    }
}

/// Whether the candidate's evidence is newer than every target's.
fn newer_evidence(note: &NoteSnapshot, targets: &[&NoteSnapshot], ctx: &Context) -> bool {
    let evidence = &ctx.snapshot.evidence;
    let Some(mine) = last_verified(&note.lifecycle, evidence, ctx.now)
        .ok()
        .flatten()
    else {
        return false;
    };
    !targets.is_empty()
        && targets.iter().all(|t| {
            last_verified(&t.lifecycle, evidence, ctx.now)
                .ok()
                .flatten()
                .is_none_or(|theirs| mine > theirs)
        })
}

/// Reserve against the context ledger, or use events the deferred queue
/// already reserved for exactly these targets.
fn reserve(
    ctx: &Context,
    requests: &[LossRequest],
    actor: Actor,
    counts: &ActiveCounts,
) -> Result<Vec<LossEvent>, BudgetExhausted> {
    if let Some(reserved) = ctx.reserved {
        let covered = requests.iter().all(|r| {
            reserved
                .iter()
                .any(|e| e.target == r.target && e.kind == r.kind)
        });
        if covered {
            return Ok(reserved.to_vec());
        }
    }
    let empty = crate::budget::BudgetLedger::default();
    let ledger = ctx.ledger.unwrap_or(&empty);
    ledger
        .reserve(requests, actor, counts, ctx.now)
        .map(|(_, events)| events)
}

pub(crate) fn loss_request(target: &NoteSnapshot, kind: LossKind) -> LossRequest {
    LossRequest {
        target: target.path.clone(),
        entities: target.entity_ids(),
        kind,
    }
}

fn adopt(
    note: &NoteSnapshot,
    ctx: &Context,
    policy: &Policy,
    gates: GateReport,
    targets: &[&NoteSnapshot],
) -> AdoptionOutcome {
    let requests: Vec<LossRequest> = targets
        .iter()
        .map(|t| loss_request(t, LossKind::Supersede))
        .collect();
    let actor = actor_of(note);
    let counts = ActiveCounts::from_snapshot(ctx.snapshot, policy);
    let reserved = match reserve(ctx, &requests, actor, &counts) {
        Ok(events) => events,
        Err(exhausted) => return deferred(note, ctx, gates, requests, actor, exhausted, targets),
    };
    let mut edits = vec![
        NoteEdit::new(&note.path)
            .with("status", json!("active"))
            .with("adoption", adoption_value(ctx, &gates, true)),
    ];
    edits.extend(
        targets
            .iter()
            .map(|t| NoteEdit::new(&t.path).with("status", json!("superseded"))),
    );
    AdoptionOutcome::Active {
        edits,
        gates,
        losses: reserved,
    }
}

fn deferred(
    note: &NoteSnapshot,
    ctx: &Context,
    gates: GateReport,
    requests: Vec<LossRequest>,
    actor: Actor,
    exhausted: BudgetExhausted,
    targets: &[&NoteSnapshot],
) -> AdoptionOutcome {
    let entry = DeferredEntry {
        id: crate::work::work_id(WorkKind::Deferred, &note.path, &[]),
        action: DeferredAction::Adopt {
            candidate: note.path.clone(),
        },
        requests,
        actor,
        enqueued_at: ctx.now,
        newer_evidence: newer_evidence(note, targets, ctx),
        target_hash: None,
    };
    let retry = exhausted
        .retry_at
        .map(format_time)
        .unwrap_or_else(|| "no automatic retry".into());
    let work = WorkItem::new(
        WorkKind::Deferred,
        &note.path,
        targets.iter().map(|t| t.path.clone()).collect(),
        format!(
            "budget-exhausted: {} spent operations; earliest retry {retry}",
            exhausted.spent.len()
        ),
        "the deferred queue resumes this automatically when the window allows",
        Trigger::Cooldown,
        ctx.now,
    )
    .with_deferred(entry.clone());
    let edits = vec![
        NoteEdit::new(&note.path)
            .with("status", json!("candidate"))
            .with("adoption", adoption_value(ctx, &gates, false)),
    ];
    AdoptionOutcome::Deferred {
        edits,
        gates,
        exhausted,
        entry,
        work: vec![work],
    }
}

/// Edits for a file-ingress (user) save: a claim-content change sets
/// `origin.kind=user` and resets a non-terminal note to `candidate`.
pub fn on_file_ingress(
    previous: Option<&NoteSnapshot>,
    current: &NoteSnapshot,
    now: crate::time::Timestamp,
    policy: &Policy,
) -> Option<NoteEdit> {
    if !policy.lifecycle_on || !current.is_memory() {
        return None;
    }
    let changed = previous.is_none_or(|p| p.claim_content != current.claim_content);
    if !changed {
        return None;
    }
    let origin = json!({"kind": "user", "locator": "file", "captured_at": format_time(now)});
    let edit = NoteEdit::new(&current.path).with("origin", origin);
    let terminal = matches!(
        current.lifecycle.status,
        Some(Status::Superseded) | Some(Status::Rejected)
    );
    Some(if terminal {
        edit
    } else {
        edit.with("status", json!("candidate"))
    })
}

/// Conflict propagation between two active claims. Flags are charged
/// against the change cap; an exhausted budget defers the flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ConflictPlan {
    pub edits: Vec<NoteEdit>,
    pub work: Vec<WorkItem>,
    pub losses: Vec<LossEvent>,
    pub deferred: Vec<DeferredEntry>,
}

pub fn propagate_conflicts(ctx: &Context, policy: &Policy) -> ConflictPlan {
    let mut plan = ConflictPlan::default();
    let empty = Default::default();
    let mut ledger = ctx.ledger.unwrap_or(&empty).clone();
    let mut counts = ActiveCounts::from_snapshot(ctx.snapshot, policy);
    let mut flags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for conflict in active_conflicts(ctx.snapshot, policy) {
        let (Some(a), Some(b)) = (ctx.snapshot.get(&conflict.a), ctx.snapshot.get(&conflict.b))
        else {
            continue;
        };
        let flagged = |x: &NoteSnapshot, y: &NoteSnapshot| {
            x.lifecycle
                .conflict_with
                .iter()
                .flatten()
                .any(|n| y.is_named(n))
        };
        if flagged(a, b) && flagged(b, a) {
            continue;
        }
        // One conflict is one action: a single loss charged once against
        // each entity of either note.
        let mut entities = a.entity_ids();
        entities.extend(b.entity_ids());
        entities.sort();
        entities.dedup();
        let requests = vec![LossRequest {
            target: conflict.a.clone(),
            entities,
            kind: LossKind::Conflict,
        }];
        let work = WorkItem::new(
            WorkKind::Conflict,
            &conflict.a,
            vec![conflict.b.clone()],
            format!(
                "active claims conflict on slot {}; neither wins",
                conflict.slot
            ),
            "decide which claim holds and edit or supersede the other",
            Trigger::SourceChange,
            ctx.now,
        );
        match ledger.reserve(&requests, Actor::Engine, &counts, ctx.now) {
            Ok((next, events)) => {
                ledger = next;
                counts = counts.after(&requests);
                plan.losses.extend(events);
                flags
                    .entry(conflict.a.clone())
                    .or_default()
                    .push(conflict.b.clone());
                flags
                    .entry(conflict.b.clone())
                    .or_default()
                    .push(conflict.a.clone());
                plan.work.push(work);
            }
            Err(_) => {
                let entry = DeferredEntry {
                    id: crate::work::work_id(
                        WorkKind::Deferred,
                        &conflict.a,
                        std::slice::from_ref(&conflict.b),
                    ),
                    action: DeferredAction::Loss,
                    requests,
                    actor: Actor::Engine,
                    enqueued_at: ctx.now,
                    newer_evidence: false,
                    target_hash: None,
                };
                plan.work.push(work.with_deferred(entry.clone()));
                plan.deferred.push(entry);
            }
        }
    }
    for (path, partners) in flags {
        let existing = ctx
            .snapshot
            .get(&path)
            .and_then(|n| n.lifecycle.conflict_with.clone())
            .unwrap_or_default();
        let mut all: Vec<String> = existing.into_iter().chain(partners).collect();
        all.sort();
        all.dedup();
        plan.edits
            .push(NoteEdit::new(path).with("conflict_with", json!(all)));
    }
    plan
}

/// `wi_work`: open a conflict work item for a pair the slot rule cannot see.
pub fn open_conflict_work(a: &str, b: &str, reason: &str, now: crate::time::Timestamp) -> WorkItem {
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    WorkItem::new(
        WorkKind::Conflict,
        first,
        vec![second.to_string()],
        reason,
        "decide which claim holds; no claim wins by time or vote",
        Trigger::SourceChange,
        now,
    )
}

/// Supersede recovery: an active note's valid active targets are marked
/// `superseded`. Not charged again: the adoption reserved the budget.
pub fn recover_supersedes(ctx: &Context, policy: &Policy) -> Vec<NoteEdit> {
    if !policy.lifecycle_on {
        return Vec::new();
    }
    let mut targets: Vec<String> = ctx
        .snapshot
        .notes()
        .iter()
        .filter(|n| competes(n, policy))
        .flat_map(|n| {
            n.supersedes()
                .into_iter()
                .filter_map(|name| ctx.snapshot.resolve(&name))
                .filter(|t| competes(t, policy))
                .filter(|t| crate::conflict::structurally_valid(n, t).is_ok())
                .map(|t| t.path.clone())
                .collect::<Vec<_>>()
        })
        .collect();
    targets.sort();
    targets.dedup();
    targets
        .into_iter()
        .map(|t| NoteEdit::new(t).with("status", json!("superseded")))
        .collect()
}

/// A visibility-loss request from a tool or the engine (archive, move into
/// the archive, exclude, supersede).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LossAction {
    pub kind: LossKind,
    pub target: String,
    pub actor: Actor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "decision")]
pub enum LossDecision {
    /// Apply `edit` (if any) and record `losses`.
    Allowed {
        edit: Option<NoteEdit>,
        losses: Vec<LossEvent>,
    },
    Refused {
        reason: String,
    },
    Deferred {
        exhausted: BudgetExhausted,
        entry: DeferredEntry,
        work: Box<WorkItem>,
    },
}

fn loss_edit(kind: LossKind, path: &str) -> Option<NoteEdit> {
    let edit = NoteEdit::new(path);
    match kind {
        LossKind::Archive | LossKind::MoveToArchive => Some(edit.with("status", json!("archived"))),
        LossKind::Exclude => Some(edit.with("excluded", json!(true))),
        LossKind::Supersede => Some(edit.with("status", json!("superseded"))),
        LossKind::Conflict => None,
    }
}

pub fn authorize_loss(action: &LossAction, ctx: &Context, policy: &Policy) -> LossDecision {
    let Some(target) = ctx.snapshot.resolve(&action.target) else {
        return LossDecision::Refused {
            reason: format!("unresolved target {}", action.target),
        };
    };
    let edit =
        loss_edit(action.kind, &target.path).filter(|_| target.is_memory() && policy.lifecycle_on);
    if action.actor == Actor::User || !policy.lifecycle_on {
        return LossDecision::Allowed {
            edit,
            losses: Vec::new(),
        };
    }
    if target.is_memory() && target.is_rule_like() {
        return LossDecision::Refused {
            reason: "rule-retirement: a rule or preference loses visibility only through user file edits"
                .into(),
        };
    }
    let current = crate::eligibility::eligibility(target, ctx, policy).is_current();
    if !current {
        return LossDecision::Allowed {
            edit,
            losses: Vec::new(),
        };
    }
    let requests = vec![loss_request(target, action.kind)];
    let counts = ActiveCounts::from_snapshot(ctx.snapshot, policy);
    match reserve(ctx, &requests, action.actor, &counts) {
        Ok(losses) => LossDecision::Allowed { edit, losses },
        Err(exhausted) => {
            let entry = DeferredEntry {
                id: crate::work::work_id(WorkKind::Deferred, &target.path, &[]),
                action: DeferredAction::Loss,
                requests,
                actor: action.actor,
                enqueued_at: ctx.now,
                newer_evidence: false,
                target_hash: None,
            };
            let work = WorkItem::new(
                WorkKind::Deferred,
                &target.path,
                Vec::new(),
                format!(
                    "budget-exhausted: {} spent operations",
                    exhausted.spent.len()
                ),
                "the deferred queue resumes this automatically when the window allows",
                Trigger::Cooldown,
                ctx.now,
            )
            .with_deferred(entry.clone());
            LossDecision::Deferred {
                exhausted,
                entry,
                work: Box::new(work),
            }
        }
    }
}
