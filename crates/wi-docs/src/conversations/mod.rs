//! Claude and ChatGPT conversation exports, imported as plain notes.
//! The parsers are tolerant of unknown and missing keys; a conversation that
//! cannot be read is skipped and counted, never fatal to the whole import.
mod chatgpt;
mod claude;
mod render;
mod run;
mod source;
mod write;

use chrono::{DateTime, Utc};

pub(crate) use run::try_import;
pub(crate) use source::Limits;

use crate::Error;

/// Owner-facing refusals start with this marker so the CLI can show the
/// sentence as written.
const MARK: &str = "conversation export: ";

/// The plain sentence of a refused conversation import, if `error` is one.
pub fn owner_sentence(error: &Error) -> Option<&str> {
    match error {
        Error::Invalid(reason) => reason.strip_prefix(MARK),
        _ => None,
    }
}

pub(crate) fn refuse(sentence: impl AsRef<str>) -> Error {
    Error::Invalid(format!("{MARK}{}", sentence.as_ref()))
}

/// Lightweight name-based detection for callers which expand directory
/// inputs. Does not follow symlinks; import repeats every check.
pub fn is_export(root: &std::path::Path, source: &str) -> bool {
    source::is_export(root, source)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Claude,
    ChatGpt,
}

impl Kind {
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::ChatGpt => "chatgpt",
        }
    }
    pub(crate) fn folder(self) -> &'static str {
        match self {
            Kind::Claude => "AI/Claude",
            Kind::ChatGpt => "AI/ChatGPT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

impl Role {
    pub(crate) fn heading(self) -> &'static str {
        match self {
            Role::User => "## User",
            Role::Assistant => "## Assistant",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Message {
    pub role: Role,
    pub text: String,
    pub attachments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Conversation {
    pub id: String,
    pub title: String,
    pub created: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
    pub messages: Vec<Message>,
}

/// One Claude project document.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectDoc {
    pub project: String,
    pub project_id: String,
    pub id: String,
    pub filename: String,
    pub content: String,
    pub created: Option<DateTime<Utc>>,
}

/// Counts reported by a conversation import. Notes are counted, so a
/// conversation split into three parts adds three.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConversationCounts {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// Notes a person edited; they were left as they are.
    pub conflicts: usize,
    /// Conversations or documents with nothing to import or that could not be read.
    pub skipped: usize,
    /// Secret-like strings replaced with `[redacted]`.
    pub redacted: usize,
    /// Vault paths of the notes counted as conflicts.
    pub conflict_paths: Vec<String>,
}

pub(crate) fn from_epoch(seconds: f64) -> Option<DateTime<Utc>> {
    if !seconds.is_finite() {
        return None;
    }
    let whole = seconds.floor();
    let nanos = ((seconds - whole) * 1e9).round().min(999_999_999.0) as u32;
    DateTime::from_timestamp(whole as i64, nanos)
}

pub(crate) fn from_rfc3339(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// A stable stand-in id for a conversation that carries none.
pub(crate) fn fallback_id(title: &str, created: Option<DateTime<Utc>>) -> String {
    let seed = format!("{title}\n{}", created.map(|c| c.timestamp()).unwrap_or(0));
    let hash = wi_history::hash_bytes(seed.as_bytes());
    format!(
        "anon-{}",
        hash.trim_start_matches("sha256:").get(..16).unwrap_or("0")
    )
}

/// Lenient field readers shared by both parsers.
pub(crate) mod lenient {
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    pub fn string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::String(s) => Some(s),
            _ => None,
        })
    }

    pub fn number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::Number(n) => n.as_f64(),
            _ => None,
        })
    }

    pub fn list<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
    }
}
