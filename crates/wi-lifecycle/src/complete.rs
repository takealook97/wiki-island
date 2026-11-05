//! User-origin auto-completion.
//!
//! An owner-written `type: memory` note that lacks classification fields is
//! completed with defaults so it can adopt with no further owner action.
//! Only top-level keys that are absent or null are filled; a present value
//! always wins. Adoption completes only user-origin notes; an agent's new
//! claim is completed once, at write time, by [`complete_agent_defaults`].
//! Origin-sensitive fields (`origin`, `status`, `adoption`, `sources`) are
//! never filled here.

use serde_json::{Value, json};
use wi_note::{Applicability, ClaimKind, Kind, OriginKind};

use crate::model::NoteSnapshot;

/// `scope` written into a completed user claim.
pub const DEFAULT_USER_SCOPE: &str = "personal";

/// Longest default `summary`, in characters.
pub const SUMMARY_MAX_CHARS: usize = 200;

/// The default `summary`: the first body line that is not empty once its
/// Markdown heading, list and quote markers are stripped, truncated to
/// [`SUMMARY_MAX_CHARS`] characters. An empty body falls back to `title`,
/// then to the file stem of `path`.
pub fn summary_from_body(body: &str, title: Option<&str>, path: &str) -> String {
    let line = body
        .lines()
        .map(strip_markers)
        .find(|line| !line.is_empty())
        .or_else(|| title.map(str::trim).filter(|t| !t.is_empty()))
        .unwrap_or_else(|| file_stem(path));
    line.chars().take(SUMMARY_MAX_CHARS).collect()
}

/// Strip leading `#`..`######`, `-`, `*`, `+`, `>` and ordinal markers,
/// repeatedly, so `> - item` and `## 1. step` reduce to their text. A marker
/// counts only when whitespace or the end of the line follows it (ordinals:
/// whitespace), so `-5 degrees` and `3.14 is` keep their text.
/// Longer digit runs read as years or quantities (`2026. year`), not list ordinals.
const MAX_ORDINAL_DIGITS: usize = 3;

fn strip_markers(line: &str) -> &str {
    let mut rest = line.trim();
    loop {
        match strip_marker(rest) {
            Some(stripped) => rest = stripped.trim_start(),
            None => return rest.trim(),
        }
    }
}

fn strip_marker(line: &str) -> Option<&str> {
    strip_symbol(line, &['-', '*', '+', '>'], 1)
        .or_else(|| strip_rule(line))
        .or_else(|| strip_symbol(line, &['#'], 6))
        .or_else(|| strip_ordinal(line))
}

/// Strip a run of up to `max` identical marker characters when whitespace or
/// the end of the line follows it.
fn strip_symbol<'a>(line: &'a str, symbols: &[char], max: usize) -> Option<&'a str> {
    let first = line.chars().next().filter(|c| symbols.contains(c))?;
    let run = line.chars().take_while(|c| *c == first).count();
    let rest = &line[run..];
    (run <= max && rest.chars().next().is_none_or(char::is_whitespace)).then_some(rest)
}

/// A line made only of one repeated list character (`---`, `***`) is a
/// thematic break and reduces to nothing.
fn strip_rule(line: &str) -> Option<&str> {
    let first = line
        .chars()
        .next()
        .filter(|c| matches!(c, '-' | '*' | '+'))?;
    line.chars().all(|c| c == first).then_some("")
}

fn strip_ordinal(line: &str) -> Option<&str> {
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || digits > MAX_ORDINAL_DIGITS {
        return None;
    }
    let rest = line[digits..].strip_prefix(['.', ')'])?;
    rest.chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then_some(rest)
}

fn file_stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name)
}

fn is_user_memory(note: &NoteSnapshot) -> bool {
    note.is_memory()
        && note.unparseable.is_none()
        && note
            .lifecycle
            .origin
            .as_ref()
            .is_some_and(|o| o.kind == OriginKind::User)
}

/// The completed snapshot and the keys to write with their values, in the
/// completion order. `applicability` and `sources` are completed
/// for the gates but not written: absent, they already read as "always
/// applies" and "self-attested" for a user-origin note. A note that is not a user-origin memory
/// note is returned unchanged with no keys. The caller runs this only with
/// lifecycle on.
pub fn complete_user_defaults(note: &NoteSnapshot) -> (NoteSnapshot, Vec<(&'static str, Value)>) {
    if !is_user_memory(note) {
        return (note.clone(), Vec::new());
    }
    let (mut next, filled) = classification_defaults(note);
    if note.lifecycle.applicability.is_none() {
        next.applicability = Some(empty_applicability());
    }
    if note.lifecycle.sources.is_none() {
        next.sources = Some(Vec::new());
    }
    let completed = NoteSnapshot {
        lifecycle: next,
        ..note.clone()
    };
    (completed, filled)
}

/// The keys to write into an agent's new `type: memory` note, in order:
/// the same classification defaults a user claim gets, plus an explicit
/// empty `applicability` (an agent claim has no implicit one). The gates
/// are unchanged: sources still have to resolve. Empty for a note that is
/// not a parseable memory note.
pub fn complete_agent_defaults(note: &NoteSnapshot) -> Vec<(&'static str, Value)> {
    if !note.is_memory() || note.unparseable.is_some() {
        return Vec::new();
    }
    let (_, mut filled) = classification_defaults(note);
    if note.lifecycle.applicability.is_none() {
        filled.push(("applicability", json!({"conditions": [], "supersedes": []})));
    }
    filled
}

fn empty_applicability() -> Applicability {
    Applicability {
        from: None,
        until: None,
        conditions: Some(Vec::new()),
        supersedes: Some(Vec::new()),
    }
}

/// `kind`, `claim_kind`, `summary`, `entities` and `scope` defaults for
/// the absent ones: the completed lifecycle and the keys to write.
fn classification_defaults(
    note: &NoteSnapshot,
) -> (wi_note::Lifecycle, Vec<(&'static str, Value)>) {
    let lc = &note.lifecycle;
    let mut filled: Vec<(&'static str, Value)> = Vec::new();
    let mut next = lc.clone();
    if lc.kind.is_none() {
        next.kind = Some(Kind::Fact);
        filled.push(("kind", json!("fact")));
    }
    if lc.claim_kind.is_none() {
        next.claim_kind = Some(ClaimKind::Historical);
        filled.push(("claim_kind", json!("historical")));
    }
    if lc.summary.is_none() {
        let summary = note
            .default_summary
            .clone()
            .unwrap_or_else(|| summary_from_body("", None, &note.path));
        filled.push(("summary", json!(summary)));
        next.summary = Some(summary);
    }
    if lc.entities.is_none() {
        next.entities = Some(Vec::new());
        filled.push(("entities", json!([])));
    }
    if lc.scope.is_none() {
        next.scope = Some(DEFAULT_USER_SCOPE.to_string());
        filled.push(("scope", json!(DEFAULT_USER_SCOPE)));
    }
    (next, filled)
}
