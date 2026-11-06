//! An agent's new memory note, completed at write time (MCP ingress).
//!
//! An agent cites sources by reference only: a vault path, optionally with
//! `#L<a>-L<b>` lines or `#<heading>`, or the path of a captured URL
//! snapshot. The engine reads each reference and writes `source_hash`. A
//! bare file reference pins the whole file by hash and attaches no span. A
//! reference with lines or a heading attaches that span with its
//! `span_hash`, capped at [`SPAN_MAX_BYTES`] and cut on a line boundary
//! (`cut: true` in the span), so a short claim always fits a response. A
//! captured URL snapshot keeps a capped span of its body, because only a
//! span locator carries the snapshot's URL. The engine also fills the
//! classification fields a user claim gets by default. The gates are unchanged: a reference
//! that does not resolve stays in the note with an empty hash, so the
//! sources gate names it and the note stays a candidate.
//!
//! An agent's rule or preference is never adopted (origin rule). It is kept
//! instead as a plain "noted" note under `rules/` or `preferences/`,
//! labelled `noted_by: agent`.

use serde_json::{Value, json};
use wi_lifecycle::{NoteSnapshot, complete_agent_defaults, sha256, summary_from_body};
use wi_note::{Note, set_key};

use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::vault::Vault;

/// Folder of noted agent rules.
pub const NOTED_RULES_DIR: &str = "rules";
/// Folder of noted agent preferences.
pub const NOTED_PREFERENCES_DIR: &str = "preferences";

/// A rule or preference an agent stated, kept as a plain note. It is
/// never a protected rule; a brief lists it in its own labelled section.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NotedNote {
    pub path: String,
    /// `rule` or `preference`.
    pub kind: String,
    pub title: Option<String>,
    pub noted_at: Option<String>,
    /// The note body.
    pub content: String,
}

/// Highest numeric suffix tried for a free noted path.
const MAX_SUFFIX: usize = 1000;

/// Longest span attached to an automatically resolved reference, in bytes.
pub const SPAN_MAX_BYTES: usize = 1024;

/// A span to attach: byte offsets and whether it was cut to the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttachSpan {
    start: usize,
    end: usize,
    cut: bool,
}

/// A resolved reference: the vault path, its bytes and the span to deliver.
struct Resolved {
    path: String,
    bytes: Vec<u8>,
    span: Option<(usize, usize)>,
}

fn edit_error(key: &str, e: impl std::fmt::Display) -> StoreError {
    StoreError::Edit(format!("cannot complete {key} for this claim: {e}"))
}

/// `rule` or `preference` when the note is a memory claim of that kind.
pub(crate) fn rule_kind(note: &Note) -> Option<&'static str> {
    let fm = note.frontmatter()?;
    if fm
        .get("type")
        .and_then(|v| v.as_str().map(str::to_owned))
        .as_deref()
        != Some("memory")
    {
        return None;
    }
    match fm
        .get("kind")
        .and_then(|v| v.as_str().map(str::to_owned))
        .as_deref()
    {
        Some("rule") => Some("rule"),
        Some("preference") => Some("preference"),
        _ => None,
    }
}

/// Byte offsets of each line start, plus the end of the text.
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    if starts.last() != Some(&text.len()) {
        starts.push(text.len());
    }
    starts
}

/// `L3`, `L3-L5` or `L3-5` (1-based, inclusive) as a byte span.
fn line_span(text: &str, fragment: &str) -> Option<(usize, usize)> {
    let rest = fragment.strip_prefix(['L', 'l'])?;
    let (a, b) = match rest.split_once('-') {
        Some((a, b)) => (a, b.trim_start_matches(['L', 'l'])),
        None => (rest, rest),
    };
    let (a, b): (usize, usize) = (a.parse().ok()?, b.parse().ok()?);
    let starts = line_starts(text);
    let lines = starts.len() - 1;
    if a == 0 || a > b || b > lines {
        return None;
    }
    Some((starts[a - 1], starts[b]))
}

fn heading_level(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    let rest = &line[level..];
    ((1..=6).contains(&level) && (rest.is_empty() || rest.starts_with([' ', '\t'])))
        .then(|| (level, rest.trim()))
}

fn same_heading(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().to_lowercase().replace('-', " ");
    norm(a) == norm(b)
}

/// The section under the heading named `fragment`, through the line
/// before the next heading of the same or a higher level.
fn heading_span(text: &str, fragment: &str) -> Option<(usize, usize)> {
    let starts = line_starts(text);
    let line = |i: usize| text[starts[i]..starts[i + 1]].trim_end_matches(['\n', '\r']);
    let lines = starts.len() - 1;
    let (first, level) = (0..lines).find_map(|i| {
        heading_level(line(i))
            .filter(|(_, title)| same_heading(title, fragment))
            .map(|(level, _)| (i, level))
    })?;
    let end = (first + 1..lines)
        .find(|&i| heading_level(line(i)).is_some_and(|(l, _)| l <= level))
        .map_or(text.len(), |i| starts[i]);
    Some((starts[first], end))
}

/// The note body without surrounding blank space; the whole text when
/// the body is empty.
fn body_span(bytes: &[u8], text: &str) -> (usize, usize) {
    let start = Note::parse(bytes).map_or(0, |n| n.body_start());
    let body = &text[start..];
    let lead = body.len() - body.trim_start().len();
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return (0, text.len());
    }
    (start + lead, start + lead + trimmed.len())
}

/// `start..end` of `text` capped at [`SPAN_MAX_BYTES`]: cut after the last
/// whole line that fits, or on a character boundary when even the first
/// line is longer than the cap.
fn capped(text: &str, (start, end): (usize, usize)) -> AttachSpan {
    if end - start <= SPAN_MAX_BYTES {
        return AttachSpan {
            start,
            end,
            cut: false,
        };
    }
    let mut limit = start + SPAN_MAX_BYTES;
    while !text.is_char_boundary(limit) {
        limit -= 1;
    }
    let cut_end = text[start..limit]
        .rfind('\n')
        .map_or(limit, |i| start + i + 1);
    AttachSpan {
        start,
        end: cut_end,
        cut: true,
    }
}

impl Vault {
    /// Read the reference `text`; `None` when it names no vault file or a
    /// line range or heading the file does not have.
    fn resolve_ref(&self, text: &str) -> Option<Resolved> {
        let (path, fragment) = match text.split_once('#') {
            Some((path, fragment)) => (path, Some(fragment.trim())),
            None => (text, None),
        };
        let path = crate::paths::normalize(self.root(), path.trim()).ok()?;
        let (path, bytes) = match read_rel(self.root(), &path).ok()? {
            Some(bytes) => (path, bytes),
            // `notes/tea` names `notes/tea.md`.
            None if !path.ends_with(".md") => {
                let md = format!("{path}.md");
                let bytes = read_rel(self.root(), &md).ok()??;
                (md, bytes)
            }
            None => return None,
        };
        let span = match (
            std::str::from_utf8(&bytes),
            fragment.filter(|f| !f.is_empty()),
        ) {
            // A bare reference or a non-text file: the hash alone.
            (_, None) | (Err(_), _) => None,
            (Ok(text), Some(fragment)) => {
                Some(line_span(text, fragment).or_else(|| heading_span(text, fragment))?)
            }
        };
        Some(Resolved { path, bytes, span })
    }

    /// The pin for one `sources` entry: a bare string or a mapping with a
    /// `ref` and no `source_hash` is resolved; anything else is kept.
    fn pin_for(&self, entry: &Value) -> Value {
        let reference = match entry {
            Value::String(text) => text.clone(),
            Value::Object(map) if !map.contains_key("source_hash") => {
                match map.get("ref").and_then(Value::as_str) {
                    Some(text) => text.to_owned(),
                    None => return entry.clone(),
                }
            }
            _ => return entry.clone(),
        };
        let Some(resolved) = self.resolve_ref(&reference) else {
            // Kept so the sources gate names it; the empty hash never matches.
            return json!({"ref": reference, "commit": null, "source_hash": ""});
        };
        let url = self.observed(&resolved.path).map(|o| o.url);
        let mut pin = json!({
            "ref": resolved.path,
            "commit": null,
            "source_hash": sha256(&resolved.bytes),
        });
        let text = std::str::from_utf8(&resolved.bytes).ok();
        let span = match (resolved.span, text) {
            (Some(span), Some(text)) => Some(capped(text, span)),
            // Only a span locator carries a captured URL: keep a capped
            // span of the snapshot's body.
            (None, Some(text)) if url.is_some() => {
                Some(capped(text, body_span(&resolved.bytes, text)))
            }
            _ => None,
        };
        if let Some(AttachSpan { start, end, cut }) = span {
            let mut location = json!({"start": start, "end": end});
            if cut {
                location["cut"] = json!(true);
            }
            pin["span_hash"] = json!(sha256(&resolved.bytes[start..end]));
            pin["locator"] = json!({"span": location, "url": url});
        }
        pin
    }

    /// Complete an agent's new memory note: resolve its source references
    /// and fill the absent classification fields. Other notes are returned
    /// unchanged.
    pub(crate) fn complete_agent_claim(&self, rel: &str, note: Note) -> Result<Note> {
        let Some(fm) = note.frontmatter() else {
            return Ok(note);
        };
        if fm.is_editable().is_err()
            || fm
                .get("type")
                .and_then(|v| v.as_str().map(str::to_owned))
                .as_deref()
                != Some("memory")
        {
            return Ok(note);
        }
        let mut next = note.clone();
        if let Some(Value::Array(entries)) = fm.get("sources") {
            let pins: Vec<Value> = entries.iter().map(|e| self.pin_for(e)).collect();
            if pins != entries {
                next = set_key(&next, "sources", &Value::Array(pins))
                    .map_err(|e| edit_error("sources", e))?;
            }
        }
        let snapshot = NoteSnapshot::from_bytes(rel, next.to_bytes());
        for (key, value) in complete_agent_defaults(&snapshot) {
            next = set_key(&next, key, &value).map_err(|e| edit_error(key, e))?;
        }
        Ok(next)
    }

    /// Noted agent rules and preferences (`noted_by: agent` notes directly
    /// under `rules/` or `preferences/`), rules first, each in path order.
    pub fn noted_notes(&self) -> Result<Vec<NotedNote>> {
        let mut out = Vec::new();
        for (dir, kind) in [
            (NOTED_RULES_DIR, "rule"),
            (NOTED_PREFERENCES_DIR, "preference"),
        ] {
            let Ok(entries) = std::fs::read_dir(self.root().join(dir)) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".md") && !n.starts_with('.'))
                .collect();
            names.sort();
            for name in names {
                let path = format!("{dir}/{name}");
                let Some(bytes) = read_rel(self.root(), &path).ok().flatten() else {
                    continue;
                };
                let Ok(note) = Note::parse(&bytes) else {
                    continue;
                };
                let text = |key: &str| {
                    note.frontmatter()
                        .and_then(|f| f.get(key))
                        .and_then(|v| v.as_str().map(str::to_owned))
                };
                if text("noted_by").as_deref() != Some("agent") {
                    continue;
                }
                out.push(NotedNote {
                    path,
                    kind: kind.to_owned(),
                    title: text("title"),
                    noted_at: text("noted_at"),
                    content: note.body().trim().to_owned(),
                });
            }
        }
        Ok(out)
    }

    /// The plain note that keeps an agent's rule or preference: its path
    /// (`rules/<slug>.md` or `preferences/<slug>.md`, the first free one)
    /// and its bytes (title, `noted_by: agent`, `noted_at`, the body).
    pub(crate) fn noted_note(
        &self,
        rel: &str,
        note: &Note,
        kind: &str,
    ) -> Result<(String, Vec<u8>)> {
        let dir = if kind == "rule" {
            NOTED_RULES_DIR
        } else {
            NOTED_PREFERENCES_DIR
        };
        let stem = rel.rsplit('/').next().unwrap_or(rel);
        let stem = stem.strip_suffix(".md").unwrap_or(stem);
        let mut path = None;
        for n in 1..=MAX_SUFFIX {
            let candidate = if n == 1 {
                format!("{dir}/{stem}.md")
            } else {
                format!("{dir}/{stem}-{n}.md")
            };
            if read_rel(self.root(), &candidate)?.is_none() {
                path = Some(candidate);
                break;
            }
        }
        let path = path.ok_or_else(|| StoreError::Exists {
            path: format!("{dir}/{stem}.md"),
            current: String::new(),
        })?;
        let fm = note.frontmatter();
        let text = |key: &str| {
            fm.and_then(|f| f.get(key))
                .and_then(|v| v.as_str().map(str::to_owned))
                .filter(|s| !s.trim().is_empty())
        };
        let title = text("title")
            .or_else(|| text("summary"))
            .unwrap_or_else(|| summary_from_body(note.body(), None, rel));
        let body = note.body().trim_start_matches(['\n', '\r']);
        let bytes = format!(
            "---\ntitle: {}\nnoted_by: agent\nnoted_at: {}\n---\n\n{}",
            json!(title),
            json!(self.now_text()),
            body
        );
        Ok((path, bytes.into_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ranges_are_inclusive_and_bounded() {
        let text = "a\nbb\nccc\n";
        assert_eq!(line_span(text, "L2"), Some((2, 5)));
        assert_eq!(line_span(text, "L1-L3"), Some((0, 9)));
        assert_eq!(line_span(text, "L2-3"), Some((2, 9)));
        assert_eq!(line_span(text, "L4"), None);
        assert_eq!(line_span(text, "L3-L2"), None);
    }

    #[test]
    fn a_long_span_is_cut_after_the_last_whole_line_that_fits() {
        let line = format!("{}\n", "x".repeat(99));
        let text = line.repeat(30);
        let span = capped(&text, (0, text.len()));
        assert!(span.cut);
        assert_eq!((span.start, span.end), (0, 1000));
        assert_eq!(
            capped(&text, (100, 300)),
            AttachSpan {
                start: 100,
                end: 300,
                cut: false
            }
        );
        // One line longer than the cap is cut on a character boundary.
        let long = "가".repeat(600);
        let span = capped(&long, (0, long.len()));
        assert!(span.cut && span.end <= SPAN_MAX_BYTES && long.is_char_boundary(span.end));
    }

    #[test]
    fn a_heading_section_ends_at_the_next_peer_heading() {
        let text = "# Top\nintro\n## Steps\none\n### Detail\ntwo\n## Next\nthree\n";
        let (start, end) = heading_span(text, "steps").unwrap();
        assert_eq!(&text[start..end], "## Steps\none\n### Detail\ntwo\n");
        assert!(heading_span(text, "absent").is_none());
    }
}
