//! Model-free lint. Lint reports; it never rewrites a note.

use serde::Serialize;
use wi_note::{CaptureStatus, ClaimKind, Kind, LifecycleType, Location};

use crate::attach::{SpanProblem, span_text};
use crate::conflict::{claims_conflict, competes, validate_supersedes};
use crate::freshness::{FreshnessState, freshness, is_user_origin};
use crate::model::{Context, NoteSnapshot, Policy, sha256};
use crate::time::{IsoDuration, parse_time};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LintCode {
    /// A fact, decision or current-state claim with no source pin.
    UnsupportedClaim,
    /// All evidence for an agent claim was supplied through agent ingress.
    SourceSelfReported,
    MissingObservedAt,
    ExpiredApplicability,
    /// Stale, expired `ttl` or drifted source.
    StaleSource,
    CompetingActiveClaim,
    /// A pinned source is missing, has no snapshot, or a hash does not match.
    BrokenRequiredSource,
    /// Read-only frontmatter, unparseable times or durations, bad spans.
    UnsupportedMarkup,
    FutureTimestamp,
    /// A `resolved` capture with neither `outputs` nor `no_memory_reason`.
    UnresolvedCapture,
}

impl LintCode {
    /// Advisory findings never change gate pass/fail.
    pub fn informational(&self) -> bool {
        matches!(self, Self::SourceSelfReported)
    }

    /// Codes that a dedicated adoption gate already decides.
    pub fn covered_by_other_gate(&self) -> bool {
        matches!(
            self,
            Self::UnsupportedClaim | Self::BrokenRequiredSource | Self::CompetingActiveClaim
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct LintFinding {
    pub code: LintCode,
    pub message: String,
}

fn finding(code: LintCode, message: impl Into<String>) -> LintFinding {
    LintFinding {
        code,
        message: message.into(),
    }
}

pub fn lint(note: &NoteSnapshot, ctx: &Context, policy: &Policy) -> Vec<LintFinding> {
    if !policy.lifecycle_on {
        return Vec::new();
    }
    let mut findings = match note.lifecycle.lifecycle_type {
        LifecycleType::Memory => lint_memory(note, ctx, policy),
        LifecycleType::Capture => lint_capture(note),
        LifecycleType::Work | LifecycleType::None => Vec::new(),
    };
    if note.lifecycle.lifecycle_type != LifecycleType::None {
        findings.extend(
            note.markup
                .iter()
                .map(|m| finding(LintCode::UnsupportedMarkup, m)),
        );
        if let Some(reason) = &note.unparseable {
            findings.push(finding(LintCode::UnsupportedMarkup, reason));
        }
    }
    findings.sort();
    findings.dedup();
    findings
}

fn lint_capture(note: &NoteSnapshot) -> Vec<LintFinding> {
    let lc = &note.lifecycle;
    let has_outputs = lc.outputs.as_ref().is_some_and(|o| !o.is_empty());
    let has_reason = lc
        .no_memory_reason
        .as_ref()
        .is_some_and(|r| !r.trim().is_empty());
    if lc.capture_status == Some(CaptureStatus::Resolved) && !has_outputs && !has_reason {
        return vec![finding(
            LintCode::UnresolvedCapture,
            "resolved capture lists no outputs and no no_memory_reason",
        )];
    }
    Vec::new()
}

/// Whether a missing pin is an unsupported claim. A user-origin claim is
/// self-attested; present pins are still checked below.
fn needs_sources(note: &NoteSnapshot) -> bool {
    !is_user_origin(&note.lifecycle)
        && (matches!(note.lifecycle.kind, Some(Kind::Fact) | Some(Kind::Decision))
            || note.lifecycle.claim_kind == Some(ClaimKind::CurrentState))
}

fn lint_memory(note: &NoteSnapshot, ctx: &Context, policy: &Policy) -> Vec<LintFinding> {
    let lc = &note.lifecycle;
    let mut out = Vec::new();
    let pins = lc.sources.as_deref().unwrap_or_default();
    if needs_sources(note) && pins.is_empty() {
        out.push(finding(
            LintCode::UnsupportedClaim,
            "no resolving source pin",
        ));
    }
    if lc.claim_kind == Some(ClaimKind::CurrentState) && lc.observed_at.is_none() {
        out.push(finding(
            LintCode::MissingObservedAt,
            "current-state claim without observed_at",
        ));
    }
    if agent_only_sources(note, ctx) {
        out.push(finding(
            LintCode::SourceSelfReported,
            "the only evidence was supplied by an agent; the engine did not fetch it",
        ));
    }
    out.extend(source_findings(note, ctx));
    out.extend(time_findings(note, ctx));
    let fresh = freshness(lc, &ctx.snapshot.evidence, policy, ctx.now);
    match fresh.state {
        FreshnessState::Stale => out.push(finding(LintCode::StaleSource, "stale")),
        FreshnessState::TtlExpired => out.push(finding(LintCode::StaleSource, "ttl expired")),
        FreshnessState::Drifted(refs) => out.push(finding(
            LintCode::StaleSource,
            format!("drifted {}", refs.join(", ")),
        )),
        FreshnessState::FutureTimestamp(field) => {
            out.push(finding(LintCode::FutureTimestamp, field));
        }
        FreshnessState::Unparseable(e) => out.push(finding(LintCode::UnsupportedMarkup, e)),
        FreshnessState::MissingEvidenceTime(refs) => out.push(finding(
            LintCode::BrokenRequiredSource,
            format!("no engine evidence time {}", refs.join(", ")),
        )),
        _ => {}
    }
    out.extend(competing(note, ctx, policy));
    out
}

fn agent_only_sources(note: &NoteSnapshot, ctx: &Context) -> bool {
    if !note
        .lifecycle
        .origin
        .as_ref()
        .is_some_and(|o| o.kind == wi_note::OriginKind::Agent)
    {
        return false;
    }
    let pins = note.lifecycle.sources.as_deref().unwrap_or_default();
    !pins.is_empty()
        && pins.iter().all(|pin| {
            crate::freshness::is_url_pin(pin)
                && ctx.snapshot.evidence.get(&pin.reference).is_some_and(|s| {
                    s.hash().as_deref() == Some(pin.source_hash.as_str())
                        && s.evidence_time.is_some()
                        && s.observers(&pin.source_hash) == ["agent"]
                })
        })
}

fn source_findings(note: &NoteSnapshot, ctx: &Context) -> Vec<LintFinding> {
    let mut out = Vec::new();
    for pin in note.lifecycle.sources.iter().flatten() {
        let Some(bytes) = ctx
            .snapshot
            .evidence
            .get(&pin.reference)
            .and_then(|s| s.bytes.as_deref())
        else {
            out.push(finding(
                LintCode::BrokenRequiredSource,
                format!("missing source {}", pin.reference),
            ));
            continue;
        };
        if sha256(bytes) != pin.source_hash {
            out.push(finding(
                LintCode::BrokenRequiredSource,
                format!("source hash mismatch {}", pin.reference),
            ));
        }
        let span = pin.locator.as_ref().and_then(|l| match &l.location {
            Location::Span(span) => Some(span),
            _ => None,
        });
        if let Some(span) = span {
            let problem = span_text(bytes, span.start, span.end, pin.span_hash.as_deref()).err();
            let code = match problem {
                Some(SpanProblem::SpanHashMismatch) => LintCode::BrokenRequiredSource,
                _ => LintCode::UnsupportedMarkup,
            };
            if let Some(problem) = problem {
                out.push(finding(
                    code,
                    format!("{} {}", problem.code(), pin.reference),
                ));
            }
        }
    }
    out
}

fn time_findings(note: &NoteSnapshot, ctx: &Context) -> Vec<LintFinding> {
    let lc = &note.lifecycle;
    let mut out = Vec::new();
    if let Some(app) = &lc.applicability {
        match app.until.as_deref().map(parse_time) {
            Some(Ok(until)) if ctx.now >= until => {
                out.push(finding(
                    LintCode::ExpiredApplicability,
                    "applicability.until passed",
                ));
            }
            Some(Err(e)) => out.push(finding(LintCode::UnsupportedMarkup, e.to_string())),
            _ => {}
        }
        if let Some(Err(e)) = app.from.as_deref().map(parse_time) {
            out.push(finding(LintCode::UnsupportedMarkup, e.to_string()));
        }
    }
    if let Some(Err(e)) = lc
        .check
        .as_ref()
        .and_then(|c| c.ttl.as_deref())
        .map(IsoDuration::parse)
    {
        out.push(finding(LintCode::UnsupportedMarkup, e.to_string()));
    }
    out
}

fn competing(note: &NoteSnapshot, ctx: &Context, policy: &Policy) -> Vec<LintFinding> {
    let exempt = |other: &NoteSnapshot| {
        note.supersedes().iter().any(|name| other.is_named(name))
            && validate_supersedes(note, other, &ctx.snapshot.evidence, ctx.now).is_ok()
    };
    ctx.snapshot
        .notes()
        .iter()
        .filter(|other| competes(other, policy) && other.path != note.path && !exempt(other))
        .filter_map(|other| claims_conflict(note, other))
        .map(|c| {
            let other = if c.a == note.path { c.b } else { c.a };
            finding(
                LintCode::CompetingActiveClaim,
                format!("conflicts with {other} on slot {}", c.slot),
            )
        })
        .collect()
}
