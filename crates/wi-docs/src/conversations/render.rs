//! Note names, bodies and splitting for conversations.
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};

use super::{Conversation, Message, ProjectDoc, refuse};
use crate::{Error, Result, render as render_note};

/// Room kept for the provenance the store adds to a new note.
const ORIGIN_RESERVE: usize = 256;
/// Smallest body budget worth splitting for.
const MIN_BUDGET: usize = 64;
const MAX_NAME_BYTES: usize = 120;
const UNTITLED: &str = "Untitled";

/// The refusal for a note size limit that cannot hold a conversation.
pub(crate) fn note_limit_too_small() -> Error {
    refuse(
        "The note size limit of this vault is too small for imported conversations. Raise max_note_bytes in the vault configuration, then import again.",
    )
}

pub(crate) fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A file or folder name that is portable and not hidden. Characters that
/// cannot appear in a name become spaces.
pub(crate) fn clean_name(title: &str) -> String {
    let spaced: String = title
        .chars()
        .map(|c| {
            if c.is_control()
                || matches!(
                    c,
                    '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '[' | ']' | '#'
                )
            {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut name = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    while name.len() > MAX_NAME_BYTES {
        name.pop();
    }
    let name = name.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if name.is_empty() {
        return UNTITLED.to_owned();
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        format!("{name} note")
    } else {
        name.to_owned()
    }
}

/// `<YYYY-MM-DD> <title>`; no date prefix without a created time.
pub(crate) fn conversation_stem(conv: &Conversation) -> String {
    let title = clean_name(&conv.title);
    match conv.created {
        Some(time) => format!("{} {title}", time.format("%Y-%m-%d")),
        None => title,
    }
}

/// File stem of a project document: its file name, without a doubled `.md`.
pub(crate) fn document_stem(doc: &ProjectDoc) -> String {
    let name = doc.filename.trim();
    clean_name(
        name.strip_suffix(".md")
            .filter(|s| !s.is_empty())
            .unwrap_or(name),
    )
}

pub(crate) fn conversation_fields(kind: super::Kind, conv: &Conversation) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("type".into(), json!("note"));
    let title = conv.title.trim();
    fields.insert(
        "title".into(),
        json!(if title.is_empty() { UNTITLED } else { title }),
    );
    fields.insert("conversation_source".into(), json!(kind.slug()));
    fields.insert("conversation_id".into(), json!(conv.id));
    if let Some(time) = conv.created {
        fields.insert("conversation_created".into(), json!(timestamp(time)));
    }
    if let Some(time) = conv.updated {
        fields.insert("conversation_updated".into(), json!(timestamp(time)));
    }
    fields
}

/// Identity of a project document in notes and in the import record.
pub(crate) fn document_key(doc: &ProjectDoc) -> String {
    format!("project:{}:{}", doc.project_id, doc.id)
}

pub(crate) fn document_fields(doc: &ProjectDoc) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("type".into(), json!("note"));
    fields.insert("title".into(), json!(doc.filename.trim()));
    fields.insert("conversation_source".into(), json!("claude"));
    fields.insert("conversation_id".into(), json!(document_key(doc)));
    fields.insert("claude_project".into(), json!(doc.project.trim()));
    if let Some(time) = doc.created {
        fields.insert("conversation_created".into(), json!(timestamp(time)));
    }
    fields
}

fn section(message: &Message) -> String {
    let mut out = String::from(message.role.heading());
    let text = message.text.trim_matches(['\n', '\r']);
    if !text.is_empty() {
        out.push_str("\n\n");
        out.push_str(text);
    }
    for name in &message.attachments {
        out.push_str(&format!("\n\nAttachment: {name}"));
    }
    out
}

/// Message sections, each within `budget`; a larger message is split at line
/// boundaries and every piece repeats the heading.
pub(crate) fn message_sections(messages: &[Message], budget: usize) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| {
            let whole = section(message);
            if whole.len() <= budget {
                return vec![whole];
            }
            let heading = message.role.heading();
            let room = budget.saturating_sub(heading.len() + 2).max(1);
            let mut body = message.text.trim_matches(['\n', '\r']).to_owned();
            for name in &message.attachments {
                body.push_str(&format!("\n\nAttachment: {name}"));
            }
            split_lines(&body, room)
                .into_iter()
                .map(|piece| format!("{heading}\n\n{piece}"))
                .collect()
        })
        .collect()
}

/// Pieces of `text` of at most `limit` bytes, cut at line ends where possible.
pub(crate) fn split_lines(text: &str, limit: usize) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        let mut rest = line;
        while current.len() + rest.len() > limit {
            if current.is_empty() {
                let mut cut = limit.min(rest.len());
                while !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                if cut == 0 {
                    cut = rest.chars().next().map_or(rest.len(), char::len_utf8);
                }
                pieces.push(rest[..cut].to_owned());
                rest = &rest[cut..];
            } else {
                pieces.push(std::mem::take(&mut current));
            }
        }
        current.push_str(rest);
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

/// What the writer needs for one conversation or document.
pub(crate) struct Draft {
    pub fields: Map<String, Value>,
    /// Builds sections for a given body budget.
    pub sections: Box<dyn Fn(usize) -> Vec<String>>,
    pub separator: &'static str,
}

/// The note texts for a draft, split to fit `bound` bytes, without their
/// checksum key.
pub(crate) fn parts(draft: &Draft, bound: usize) -> Result<Vec<String>> {
    let mut probe = draft.fields.clone();
    probe.insert("conversation_part".into(), json!(99));
    probe.insert("conversation_parts".into(), json!(99));
    probe.insert(
        "conversation_checksum".into(),
        json!(format!("sha256:{}", "0".repeat(64))),
    );
    let overhead = render_note(Value::Object(probe), "")?.len() + ORIGIN_RESERVE;
    let budget = bound.saturating_sub(overhead);
    if budget < MIN_BUDGET {
        return Err(note_limit_too_small());
    }
    let sections = (draft.sections)(budget);
    let mut bodies: Vec<String> = Vec::new();
    let mut current = String::new();
    for piece in sections {
        let extra = if current.is_empty() {
            0
        } else {
            draft.separator.len()
        };
        if !current.is_empty() && current.len() + extra + piece.len() > budget {
            bodies.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str(draft.separator);
        }
        current.push_str(&piece);
    }
    if !current.is_empty() || bodies.is_empty() {
        bodies.push(current);
    }
    let total = bodies.len();
    bodies
        .into_iter()
        .enumerate()
        .map(|(index, body)| {
            let mut fields = draft.fields.clone();
            if total > 1 {
                fields.insert("conversation_part".into(), json!(index + 1));
                fields.insert("conversation_parts".into(), json!(total));
            }
            let body = if body.ends_with('\n') {
                body
            } else {
                format!("{body}\n")
            };
            render_note(Value::Object(fields), &body)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversations::Role;

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            text: text.into(),
            attachments: vec![],
        }
    }

    #[test]
    fn names_are_portable_and_never_empty() {
        assert_eq!(clean_name("a/b:c*d? [e] #f"), "a b c d e f");
        assert_eq!(clean_name("  ..hidden.. "), "hidden");
        assert_eq!(clean_name(""), "Untitled");
        assert_eq!(clean_name("///"), "Untitled");
        assert_eq!(clean_name("con"), "con note");
        assert!(clean_name(&"한".repeat(200)).len() <= MAX_NAME_BYTES);
        assert_eq!(clean_name("line\nbreak"), "line break");
    }

    #[test]
    fn stem_uses_the_utc_date() {
        let conv = Conversation {
            id: "1".into(),
            title: "Plan".into(),
            created: crate::conversations::from_rfc3339("2025-03-04T23:30:00-05:00"),
            updated: None,
            messages: vec![],
        };
        assert_eq!(conversation_stem(&conv), "2025-03-05 Plan");
        let undated = Conversation {
            created: None,
            ..conv
        };
        assert_eq!(conversation_stem(&undated), "Plan");
    }

    #[test]
    fn long_text_splits_at_lines_then_characters() {
        assert_eq!(split_lines("aaa\nbbb\nccc\n", 8), ["aaa\nbbb\n", "ccc\n"]);
        assert_eq!(split_lines("abcdefgh", 3), ["abc", "def", "gh"]);
        let wide = split_lines("한국어", 4);
        assert_eq!(wide, ["한", "국", "어"]);
    }

    #[test]
    fn an_oversized_message_repeats_its_heading() {
        let big = format!("{}\n{}\n", "a".repeat(40), "b".repeat(40));
        let sections = message_sections(&[msg(Role::User, "hi"), msg(Role::Assistant, &big)], 60);
        assert_eq!(sections[0], "## User\n\nhi");
        assert!(sections.len() >= 3);
        assert!(
            sections[1..]
                .iter()
                .all(|s| s.starts_with("## Assistant\n\n") && s.len() <= 60)
        );
    }

    #[test]
    fn attachments_follow_the_text() {
        let mut m = msg(Role::User, "see");
        m.attachments = vec!["a.pdf".into()];
        assert_eq!(section(&m), "## User\n\nsee\n\nAttachment: a.pdf");
    }
}
