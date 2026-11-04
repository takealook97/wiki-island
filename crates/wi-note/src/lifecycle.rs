use crate::{Diagnostic, Note, Value};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;

pub const ENGINE_OWNED_FIELDS: &[&str] = &[
    "status",
    "adoption",
    "check",
    "conflict_with",
    "origin",
    "excluded",
    "restores",
    // Executable work and queue state: only engine operations
    // and the dedicated work commands write these.
    "work",
    "deferred",
    "work_id",
    "work_kind",
];
pub const CLAIM_CONTENT_FIELDS: &[&str] = &[
    "summary",
    "sources",
    "applicability",
    "kind",
    "claim_kind",
    "observed_at",
    "slot",
    "value",
    "entities",
    "scope",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleType {
    Memory,
    Capture,
    Work,
    #[default]
    None,
}

macro_rules! field_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $text)] $variant),+ }
    };
}
field_enum!(Status { Candidate => "candidate", Active => "active", Rejected => "rejected", Superseded => "superseded", Archived => "archived" });
field_enum!(Kind { Fact => "fact", Decision => "decision", Rule => "rule", Procedure => "procedure", Preference => "preference", Event => "event" });
field_enum!(ClaimKind { CurrentState => "current-state", Historical => "historical", Intent => "intent" });
// `engine`: text compiled into the binary (the built-in maintenance procedure).
field_enum!(OriginKind { User => "user", Agent => "agent", File => "file", Url => "url", Engine => "engine" });
field_enum!(CheckResult { Unchanged => "unchanged", Changed => "changed", Missing => "missing", Unavailable => "unavailable" });
field_enum!(CaptureStatus { Pending => "pending", Resolved => "resolved" });
field_enum!(WorkState { Ready => "ready", Investigating => "investigating", Checkpointed => "checkpointed", Closed => "closed" });
field_enum!(ConditionOp { Eq => "eq", In => "in" });

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub n: usize,
    pub label: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub page: usize,
    pub bbox: [f64; 4],
}
/// Exactly one location kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    Span(Span),
    Page(Page),
    Region(Region),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Locator {
    #[serde(flatten)]
    pub location: Location,
    pub url: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourcePin {
    #[serde(rename = "ref")]
    pub reference: String,
    pub commit: Option<String>,
    pub pending: Option<bool>,
    pub source_hash: String,
    pub span_hash: Option<String>,
    pub locator: Option<Locator>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub key: String,
    pub op: ConditionOp,
    pub values: Vec<Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Applicability {
    pub from: Option<String>,
    pub until: Option<String>,
    pub conditions: Option<Vec<Condition>>,
    pub supersedes: Option<Vec<String>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub label: Option<String>,
    pub source: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    pub kind: OriginKind,
    pub locator: Option<String>,
    pub captured_at: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub checked_at: Option<String>,
    pub result: Option<CheckResult>,
    pub integrity_at: Option<String>,
    pub volatile: Option<bool>,
    pub ttl: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Adoption {
    pub at: Option<String>,
    pub by: Option<String>,
    pub gates: Option<BTreeMap<String, Value>>,
    pub literal: Option<bool>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Restores {
    #[serde(rename = "ref")]
    pub reference: String,
    pub commit: Option<String>,
}

/// Work metadata: policy-dependent payloads remain lossless JSON values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Work {
    pub target: Option<String>,
    pub state: Option<WorkState>,
    pub next_action: Option<String>,
    pub next_trigger: Option<String>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub limits: Option<Value>,
    pub reopen: Option<Value>,
}

/// Typed read-only projection. Null and absent fields remain unresolved (`None`).
/// Parsing validates shapes, not lifecycle eligibility, timestamp policy or hash integrity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Lifecycle {
    pub lifecycle_type: LifecycleType,
    pub status: Option<Status>,
    pub claim_kind: Option<ClaimKind>,
    pub kind: Option<Kind>,
    pub summary: Option<String>,
    pub tags: Option<Vec<String>>,
    pub exclusion_reason: Option<String>,
    pub skill_id: Option<String>,
    pub capture_status: Option<CaptureStatus>,
    pub outputs: Option<Vec<String>>,
    pub no_memory_reason: Option<String>,
    pub work: Option<Work>,
    pub sources: Option<Vec<SourcePin>>,
    pub applicability: Option<Applicability>,
    pub slot: Option<String>,
    pub value: Option<Value>,
    pub observed_at: Option<String>,
    pub entities: Option<Vec<Entity>>,
    pub scope: Option<String>,
    pub origin: Option<Origin>,
    pub check: Option<Check>,
    pub adoption: Option<Adoption>,
    pub conflict_with: Option<Vec<String>>,
    pub excluded: Option<bool>,
    pub restores: Option<Restores>,
}

impl Lifecycle {
    pub(crate) fn from_note(note: &Note) -> (Self, Vec<Diagnostic>) {
        let mut diagnostics = Vec::new();
        if note.frontmatter.as_ref().is_some_and(|f| {
            f.read_only_reasons().iter().any(|r| {
                matches!(
                    r,
                    crate::ReadOnlyReason::Unterminated
                        | crate::ReadOnlyReason::InvalidYaml(_)
                        | crate::ReadOnlyReason::DuplicateKey(_)
                        | crate::ReadOnlyReason::NonMapping
                        | crate::ReadOnlyReason::MultipleDocuments
                )
            })
        }) {
            return (Self::default(), diagnostics);
        }
        let lifecycle_type = match note.frontmatter.as_ref().and_then(|f| f.get("type")) {
            Some(Value::String(s)) => match s.as_str() {
                "memory" => LifecycleType::Memory,
                "capture" => LifecycleType::Capture,
                "work" => LifecycleType::Work,
                _ => LifecycleType::None,
            },
            Some(Value::Null) | None => LifecycleType::None,
            _ => {
                diagnostics.push(Diagnostic {
                    field: Some("type".into()),
                    message: "expected string type".into(),
                });
                LifecycleType::None
            }
        };
        // Plain-note metadata never becomes lifecycle state merely by sharing a key name.
        if lifecycle_type == LifecycleType::None {
            return (Self::default(), diagnostics);
        }
        macro_rules! get {
            ($key:literal) => {
                field(note, $key, &mut diagnostics)
            };
        }
        // Validate ambiguous source syntax before serde's flattened enum selects a variant.
        let invalid_sources = note.frontmatter.as_ref().and_then(|f| f.get("sources"))
            .and_then(|v| v.as_array().cloned()).is_some_and(|sources| sources.iter().any(|source| {
                let Some(locator) = source.get("locator").and_then(Value::as_object) else { return false; };
                let count = ["span", "page", "region"].iter().filter(|k| locator.contains_key(**k)).count();
                let reversed = locator.get("span").is_some_and(|span| {
                    matches!((span.get("start").and_then(Value::as_u64), span.get("end").and_then(Value::as_u64)), (Some(start), Some(end)) if start > end)
                });
                count != 1 || reversed
            }));
        let mut view = Self {
            lifecycle_type,
            status: get!("status"),
            claim_kind: get!("claim_kind"),
            kind: get!("kind"),
            summary: get!("summary"),
            tags: get!("tags"),
            exclusion_reason: get!("exclusion_reason"),
            skill_id: get!("skill_id"),
            capture_status: get!("capture_status"),
            outputs: get!("outputs"),
            no_memory_reason: get!("no_memory_reason"),
            work: get!("work"),
            sources: get!("sources"),
            applicability: get!("applicability"),
            slot: get!("slot"),
            value: get!("value"),
            observed_at: get!("observed_at"),
            entities: get!("entities"),
            scope: get!("scope"),
            origin: get!("origin"),
            check: get!("check"),
            adoption: get!("adoption"),
            conflict_with: get!("conflict_with"),
            excluded: get!("excluded"),
            restores: get!("restores"),
        };
        if invalid_sources {
            diagnostics.push(Diagnostic {
                field: Some("sources".into()),
                message: "locator requires exactly one kind and span start <= end".into(),
            });
            view.sources = None;
        }
        if view
            .applicability
            .as_ref()
            .and_then(|a| a.conditions.as_ref())
            .is_some_and(|conditions| {
                conditions.iter().any(|c| match c.op {
                    ConditionOp::Eq => c.values.len() != 1,
                    ConditionOp::In => c.values.is_empty(),
                })
            })
        {
            diagnostics.push(Diagnostic {
                field: Some("applicability".into()),
                message: "eq requires exactly one value; in requires at least one value".into(),
            });
            view.applicability = None;
        }
        (view, diagnostics)
    }
}

fn field<T: DeserializeOwned>(
    note: &Note,
    key: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<T> {
    let value = note.frontmatter.as_ref()?.get(key)?;
    if value.is_null() {
        return None;
    }
    match serde_json::from_value(value) {
        Ok(value) => Some(value),
        Err(error) => {
            diagnostics.push(Diagnostic {
                field: Some(key.into()),
                message: error.to_string(),
            });
            None
        }
    }
}
