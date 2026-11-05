//! New claim revisions: MCP edits of claim content and reverts. Both build a
//! new candidate note; the old note is never changed in place.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use thiserror::Error;
use wi_note::{CLAIM_CONTENT_FIELDS, ENGINE_OWNED_FIELDS, EditError, Note, OriginKind, set_key};

use crate::time::{Timestamp, format_time};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RevisionError {
    #[error("engine-owned field {0} is not writable")]
    EngineOwned(String),
    #[error("not a claim-content field: {0}")]
    NotClaimContent(String),
    #[error("no claim-content change requested")]
    Empty,
    #[error("frontmatter edit failed: {0}")]
    Edit(String),
}

impl From<EditError> for RevisionError {
    fn from(error: EditError) -> Self {
        Self::Edit(error.to_string())
    }
}

fn set_all(note: &Note, keys: &[(&str, Value)]) -> Result<Note, RevisionError> {
    keys.iter().try_fold(note.clone(), |note, (key, value)| {
        set_key(&note, key, value).map_err(RevisionError::from)
    })
}

fn origin_text(kind: &OriginKind) -> &'static str {
    match kind {
        OriginKind::User => "user",
        OriginKind::Agent => "agent",
        OriginKind::File => "file",
        OriginKind::Url => "url",
        OriginKind::Engine => "engine",
    }
}

/// `wi_edit` on claim fields: a new agent candidate that supersedes `old_path`.
/// For a rule or preference this candidate can never adopt (origin rule).
pub fn mcp_claim_edit(
    old: &Note,
    old_path: &str,
    changes: &BTreeMap<String, Value>,
    now: Timestamp,
) -> Result<Note, RevisionError> {
    if changes.is_empty() {
        return Err(RevisionError::Empty);
    }
    mcp_revision(old, old_path, changes, now)
}

/// The fork for an MCP claim edit whose body change is already
/// applied to `old`. A memory note's body is claim content. The
/// frontmatter `changes` may be empty.
pub fn mcp_revision(
    old: &Note,
    old_path: &str,
    changes: &BTreeMap<String, Value>,
    now: Timestamp,
) -> Result<Note, RevisionError> {
    if let Some(key) = changes
        .keys()
        .find(|k| ENGINE_OWNED_FIELDS.contains(&k.as_str()))
    {
        return Err(RevisionError::EngineOwned(key.clone()));
    }
    if let Some(key) = changes
        .keys()
        .find(|k| !CLAIM_CONTENT_FIELDS.contains(&k.as_str()))
    {
        return Err(RevisionError::NotClaimContent(key.clone()));
    }
    let edited = changes.iter().try_fold(old.clone(), |note, (key, value)| {
        set_key(&note, key, value).map_err(RevisionError::from)
    })?;
    let applicability = edited
        .frontmatter()
        .and_then(|f| f.get("applicability"))
        .filter(Value::is_object)
        .unwrap_or_else(
            || json!({"from": null, "until": null, "conditions": [], "supersedes": []}),
        );
    let mut applicability = applicability;
    applicability["supersedes"] = json!([old_path]);
    set_all(
        &edited,
        &[
            ("applicability", applicability),
            ("status", json!("candidate")),
            (
                "origin",
                json!({"kind": "agent", "locator": "mcp:wi_edit", "captured_at": format_time(now)}),
            ),
            ("adoption", Value::Null),
            ("conflict_with", json!([])),
        ],
    )
}

/// A revert: a new candidate that names the earlier revision in `restores`.
/// It passes the gates again; the terminal original is never revived.
pub fn restore_revision(
    old: &Note,
    old_path: &str,
    commit: Option<&str>,
    origin: OriginKind,
    now: Timestamp,
) -> Result<Note, RevisionError> {
    set_all(
        old,
        &[
            ("status", json!("candidate")),
            ("restores", json!({"ref": old_path, "commit": commit})),
            (
                "origin",
                json!({"kind": origin_text(&origin), "locator": "revert", "captured_at": format_time(now)}),
            ),
            ("adoption", Value::Null),
            ("conflict_with", json!([])),
        ],
    )
}
