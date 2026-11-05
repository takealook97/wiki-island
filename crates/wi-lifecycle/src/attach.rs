//! Literal detection and deliverable source spans (KNOWLEDGE Adoption gate 3).
//!
//! A non-literal claim travels with its source spans, so a span that cannot
//! be produced from the evidence is a missing attachment, never a silent drop.

use wi_note::{Lifecycle, Location, SourcePin};

use crate::model::{Evidence, sha256};

/// Why a pin cannot supply an attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpanProblem {
    /// No locator: the pin supports the whole source but names no span.
    NoLocator,
    /// A page or region locator; this library cannot produce its text.
    NotSpan,
    SourceUnavailable,
    OutOfRange,
    /// The byte range is not valid UTF-8 (for example, it splits a code point).
    NotUtf8,
    NoSpanHash,
    SpanHashMismatch,
}

impl SpanProblem {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::NoLocator => "no span locator",
            Self::NotSpan => "page or region locator without text",
            Self::SourceUnavailable => "source unavailable",
            Self::OutOfRange => "span out of range",
            Self::NotUtf8 => "span not on UTF-8 boundaries",
            Self::NoSpanHash => "no span_hash",
            Self::SpanHashMismatch => "span hash mismatch",
        }
    }
}

/// Check a span's bytes: in range, valid UTF-8 and matching `span_hash` when given.
pub(crate) fn span_text<'a>(
    bytes: &'a [u8],
    start: usize,
    end: usize,
    span_hash: Option<&str>,
) -> Result<&'a str, SpanProblem> {
    let part = bytes.get(start..end).ok_or(SpanProblem::OutOfRange)?;
    let text = std::str::from_utf8(part).map_err(|_| SpanProblem::NotUtf8)?;
    match span_hash {
        Some(hash) if hash != sha256(part) => Err(SpanProblem::SpanHashMismatch),
        _ => Ok(text),
    }
}

/// The text of a pin's span, if it can be delivered: a span locator over
/// available bytes, on UTF-8 boundaries, with a present and matching `span_hash`.
pub(crate) fn deliverable_span(
    pin: &SourcePin,
    evidence: &Evidence,
) -> Result<String, SpanProblem> {
    let locator = pin.locator.as_ref().ok_or(SpanProblem::NoLocator)?;
    let Location::Span(span) = &locator.location else {
        return Err(SpanProblem::NotSpan);
    };
    let bytes = evidence
        .get(&pin.reference)
        .and_then(|s| s.bytes.as_deref())
        .ok_or(SpanProblem::SourceUnavailable)?;
    let hash = pin.span_hash.as_deref().ok_or(SpanProblem::NoSpanHash)?;
    span_text(bytes, span.start, span.end, Some(hash)).map(str::to_string)
}

/// `summary` equals the exact bytes of a pinned span whose `span_hash` matches.
pub(crate) fn is_literal(lifecycle: &Lifecycle, evidence: &Evidence) -> bool {
    let Some(summary) = lifecycle.summary.as_deref() else {
        return false;
    };
    lifecycle
        .sources
        .iter()
        .flatten()
        .any(|pin| deliverable_span(pin, evidence).is_ok_and(|text| text == summary))
}

/// Pins whose attachment a non-literal claim cannot deliver, as `ref: problem`.
/// Every pin with a locator must deliver its span. A pin without a locator
/// supports the whole source by its hash and names no span to attach, so a
/// claim citing whole files only is deliverable without attachments (the
/// sources gate still checks each hash). A claim without pins has nothing
/// to attach.
pub(crate) fn missing_attachments(lifecycle: &Lifecycle, evidence: &Evidence) -> Vec<String> {
    lifecycle
        .sources
        .iter()
        .flatten()
        .filter_map(|pin: &SourcePin| match deliverable_span(pin, evidence) {
            Ok(_) | Err(SpanProblem::NoLocator) => None,
            Err(problem) => Some(format!("{}: {}", pin.reference, problem.code())),
        })
        .collect()
}
