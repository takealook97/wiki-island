//! Input snapshots. The engine never reads files: the host passes notes,
//! source bytes, engine-recorded evidence times, loss events and `now`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wi_note::{
    CLAIM_CONTENT_FIELDS, Kind, Lifecycle, LifecycleType, Note, ReadOnlyReason, Status, Value,
    VaultConfig,
};

use crate::budget::{BudgetLedger, LossEvent};
use crate::time::{IsoDuration, Timestamp};

/// `sha256:<64 lowercase hex>` of `bytes`, the workspace hash format.
pub fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(71);
    out.push_str("sha256:");
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Vault policy read from `.wiki-island/config.toml`. All defaults are unmeasured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub lifecycle_on: bool,
    pub stale_after_days: i64,
    pub archive_dir: String,
    /// Archive directories this vault used before (the default `archive`
    /// and any directory an earlier `archive_dir` named). Notes under them
    /// stay archived when the key changes.
    pub former_archive_dirs: Vec<String>,
    pub work_dir: String,
    pub captures_dir: String,
    /// `ttl` applied to a URL-snapshot claim that carries none (default `P7D`).
    pub url_default_ttl: IsoDuration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            lifecycle_on: true,
            stale_after_days: 30,
            archive_dir: wi_note::ARCHIVE_DIR.into(),
            former_archive_dirs: Vec::new(),
            work_dir: wi_note::WORK_DIR.into(),
            captures_dir: wi_note::CAPTURES_DIR.into(),
            url_default_ttl: IsoDuration::days(7),
        }
    }
}

impl Policy {
    pub fn from_config(config: &VaultConfig) -> Self {
        Self {
            lifecycle_on: config.lifecycle() != "off",
            stale_after_days: config.stale_after_days(),
            archive_dir: config.archive_dir().to_string(),
            work_dir: config.work_dir().to_string(),
            captures_dir: config.captures_dir().to_string(),
            ..Self::default()
        }
    }

    pub fn off() -> Self {
        Self {
            lifecycle_on: false,
            ..Self::default()
        }
    }

    /// The policy with `dirs` as former archive directories (see
    /// [`Policy::former_archive_dirs`]).
    pub fn with_former_archive_dirs(mut self, dirs: Vec<String>) -> Self {
        self.former_archive_dirs = dirs;
        self
    }

    /// Any note under the archive directory, or under a former one, is
    /// archived, whatever its status key.
    pub fn is_archived_path(&self, path: &str) -> bool {
        std::iter::once(&self.archive_dir)
            .chain(&self.former_archive_dirs)
            .any(|dir| {
                let dir = dir.trim_matches('/');
                !dir.is_empty()
                    && path
                        .strip_prefix(dir)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
    }
}

/// Who performs an action. File ingress (editor, CLI) is the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    User,
    Agent,
    Engine,
}

/// Authored `type` values that make a note a lifecycle note.
pub const LIFECYCLE_TYPES: &[&str] = &["memory", "capture", "work"];

/// Format tag of a materialized snapshot in the device index. Rows with
/// another tag are ignored, so a binary whose snapshot shape or parsing
/// differs re-reads the files instead of trusting older rows.
pub const MATERIALIZED_FORMAT: &str = concat!("note-snapshot/3/", env!("CARGO_PKG_VERSION"));

/// One note as the engine sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteSnapshot {
    pub path: String,
    pub permalink: Option<String>,
    pub lifecycle: Lifecycle,
    /// Hash of the trimmed body; `None` for an empty body (no duplicate detection).
    pub body_hash: Option<String>,
    /// Why the note cannot be read as lifecycle metadata, if it cannot.
    pub unparseable: Option<String>,
    /// Read-only frontmatter reasons that lint reports as unsupported markup.
    pub markup: Vec<String>,
    /// Digest of the present claim-content values (`CLAIM_CONTENT_FIELDS`),
    /// for revision detection; `None` when none is present. A digest keeps
    /// per-note snapshot memory small.
    pub claim_content: Option<String>,
    /// `work_id` of a work note.
    pub work_id: Option<String>,
    /// `deferred` payload of a work note holding a deferred-queue entry.
    pub deferred: Option<Value>,
    /// Default `summary` for user-origin completion, derived from the body,
    /// `title` or file stem. Kept only for a memory note without `summary`,
    /// so other snapshots carry nothing extra.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_summary: Option<String>,
    /// True when the raw frontmatter has no `applicability.conditions` key
    /// (`applicability` absent, or a mapping without it). An explicit
    /// `conditions: null` is not absent (docs/KNOWLEDGE.md).
    #[serde(default, skip_serializing_if = "is_false")]
    pub conditions_absent: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Canonical digest of claim-content values; `None` for an empty map.
fn claim_digest(values: &BTreeMap<&str, Value>) -> Option<String> {
    (!values.is_empty())
        .then(|| sha256(serde_json::to_string(values).unwrap_or_default().as_bytes()))
}

impl NoteSnapshot {
    pub fn from_note(path: impl Into<String>, note: &Note) -> Self {
        let lifecycle = note.lifecycle();
        let frontmatter = note.frontmatter();
        let get = |key: &str| frontmatter.and_then(|f| f.get(key));
        let malformed = frontmatter.and_then(|f| {
            f.read_only_reasons().iter().find(|r| {
                matches!(
                    r,
                    ReadOnlyReason::Unterminated
                        | ReadOnlyReason::InvalidYaml(_)
                        | ReadOnlyReason::DuplicateKey(_)
                        | ReadOnlyReason::NonMapping
                        | ReadOnlyReason::MultipleDocuments
                )
            })
        });
        let markup = frontmatter
            .map(|f| {
                f.read_only_reasons()
                    .iter()
                    .map(|r| r.to_string())
                    .collect()
            })
            .unwrap_or_default();
        // A field-level diagnostic means a lifecycle key had the wrong shape.
        let field_error = (lifecycle.lifecycle_type != LifecycleType::None)
            .then(|| {
                note.diagnostics()
                    .iter()
                    .find(|d| d.field.is_some())
                    .map(|d| format!("{}: {}", d.field.clone().unwrap_or_default(), d.message))
            })
            .flatten();
        let body = note.body().trim();
        let path: String = path.into();
        let default_summary = (lifecycle.lifecycle_type == LifecycleType::Memory
            && lifecycle.summary.is_none())
        .then(|| {
            let title = get("title").and_then(|v| v.as_str().map(str::to_string));
            crate::complete::summary_from_body(body, title.as_deref(), &path)
        });
        let conditions_absent = match get("applicability") {
            None => true,
            Some(Value::Object(map)) => !map.contains_key("conditions"),
            Some(_) => false,
        };
        Self {
            path,
            permalink: get("permalink").and_then(|v| v.as_str().map(str::to_string)),
            body_hash: (!body.is_empty()).then(|| sha256(body.as_bytes())),
            unparseable: malformed.map(|r| r.to_string()).or(field_error),
            markup,
            claim_content: claim_digest(
                &CLAIM_CONTENT_FIELDS
                    .iter()
                    .filter_map(|k| get(k).map(|v| (*k, v)))
                    .collect(),
            ),
            work_id: get("work_id").and_then(|v| v.as_str().map(str::to_string)),
            deferred: get("deferred").filter(|v| !v.is_null()),
            default_summary,
            conditions_absent,
            lifecycle,
        }
    }

    /// Parse bytes; invalid UTF-8 yields an unparseable snapshot.
    pub fn from_bytes(path: impl Into<String>, bytes: &[u8]) -> Self {
        let path = path.into();
        match Note::parse(bytes) {
            Ok(note) => Self::from_note(path, &note),
            Err(error) => Self::unparseable(path, error.to_string()),
        }
    }

    /// Serialized form for the device index, only when it reads back as
    /// an equal snapshot (a JSON `null` cannot tell `Some(Null)` from `None`).
    pub fn materialize(&self) -> Option<String> {
        let text = serde_json::to_string(self).ok()?;
        (Self::from_materialized(&self.path, &text).as_ref() == Some(self)).then_some(text)
    }

    /// A snapshot read back from the device index; `None` when the text
    /// does not parse or names another path.
    pub fn from_materialized(path: &str, text: &str) -> Option<Self> {
        serde_json::from_str::<Self>(text)
            .ok()
            .filter(|snapshot| snapshot.path == path)
    }

    pub fn unparseable(path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            permalink: None,
            lifecycle: Lifecycle::default(),
            body_hash: None,
            unparseable: Some(reason.into()),
            markup: Vec::new(),
            claim_content: None,
            work_id: None,
            deferred: None,
            default_summary: None,
            conditions_absent: false,
        }
    }

    pub fn is_memory(&self) -> bool {
        self.lifecycle.lifecycle_type == LifecycleType::Memory
    }

    pub fn is_active(&self) -> bool {
        self.is_memory() && self.lifecycle.status == Some(Status::Active)
    }

    pub fn is_rule_like(&self) -> bool {
        matches!(
            self.lifecycle.kind,
            Some(Kind::Rule) | Some(Kind::Preference)
        )
    }

    pub fn entity_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .lifecycle
            .entities
            .iter()
            .flatten()
            .map(|e| e.id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    pub fn supersedes(&self) -> Vec<String> {
        self.lifecycle
            .applicability
            .as_ref()
            .and_then(|a| a.supersedes.clone())
            .unwrap_or_default()
    }

    /// Whether `name` (path, path without `.md`, or permalink) names this note.
    pub fn is_named(&self, name: &str) -> bool {
        let name = name.trim();
        self.path == name
            || self.path.strip_suffix(".md") == Some(name)
            || self.permalink.as_deref() == Some(name)
    }
}

/// The bytes and engine-recorded evidence of one pinned source.
#[derive(Debug, Clone, Default)]
pub struct SourceState {
    /// Current local bytes; `None` when the file or snapshot is missing.
    pub bytes: Option<Vec<u8>>,
    /// Manifest or ingest record time of the pinned blob, or snapshot capture time.
    pub evidence_time: Option<Timestamp>,
    /// URL snapshots only: supplied observation events on the current bytes,
    /// including the capture itself. Re-hashing stored bytes is not an observation.
    pub observations: Vec<Timestamp>,
    /// URL snapshot hash → distinct ingress origins of observations of those bytes.
    pub observed_by: BTreeMap<String, Vec<String>>,
}

impl SourceState {
    pub fn present(bytes: impl Into<Vec<u8>>, evidence_time: Option<Timestamp>) -> Self {
        Self {
            bytes: Some(bytes.into()),
            evidence_time,
            observations: Vec::new(),
            observed_by: BTreeMap::new(),
        }
    }

    pub fn missing() -> Self {
        Self::default()
    }

    pub fn with_observations(self, observations: Vec<Timestamp>) -> Self {
        Self {
            observations,
            ..self
        }
    }

    /// Observers of the pinned bytes, never observers of a different revision.
    pub fn observers(&self, hash: &str) -> Vec<String> {
        self.observed_by
            .get(hash)
            .filter(|origins| !origins.is_empty())
            .cloned()
            .unwrap_or_else(|| vec!["unknown".into()])
    }

    pub fn hash(&self) -> Option<String> {
        self.bytes.as_deref().map(sha256)
    }
}

/// Source states keyed by vault-relative `ref`.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub sources: BTreeMap<String, SourceState>,
}

impl Evidence {
    pub fn with(self, reference: impl Into<String>, state: SourceState) -> Self {
        let mut sources = self.sources;
        sources.insert(reference.into(), state);
        Self { sources }
    }

    pub fn get(&self, reference: &str) -> Option<&SourceState> {
        self.sources.get(reference)
    }
}

/// A consistent view of the vault: notes sorted by path plus their evidence.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    notes: Vec<NoteSnapshot>,
    pub evidence: Evidence,
}

impl Snapshot {
    pub fn new(notes: Vec<NoteSnapshot>, evidence: Evidence) -> Self {
        let mut notes = notes;
        notes.sort_by(|a, b| a.path.cmp(&b.path));
        notes.dedup_by(|a, b| a.path == b.path);
        Self { notes, evidence }
    }

    pub fn notes(&self) -> &[NoteSnapshot] {
        &self.notes
    }

    pub fn get(&self, path: &str) -> Option<&NoteSnapshot> {
        self.notes
            .binary_search_by(|n| n.path.as_str().cmp(path))
            .ok()
            .map(|i| &self.notes[i])
    }

    /// Resolve a `supersedes` or `conflict_with` name; an exact path wins.
    pub fn resolve(&self, name: &str) -> Option<&NoteSnapshot> {
        self.get(name.trim())
            .or_else(|| self.notes.iter().find(|n| n.is_named(name)))
    }

    /// The same snapshot with one note added or replaced.
    pub fn with_note(&self, note: NoteSnapshot) -> Self {
        let path = note.path.clone();
        let notes = self
            .notes
            .iter()
            .filter(|n| n.path != path)
            .cloned()
            .chain(std::iter::once(note))
            .collect();
        Self::new(notes, self.evidence.clone())
    }
}

/// Query-supplied condition values for `current` reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadQuery {
    pub conditions: BTreeMap<String, Value>,
}

impl ReadQuery {
    pub fn with(self, key: impl Into<String>, value: Value) -> Self {
        let mut conditions = self.conditions;
        conditions.insert(key.into(), value);
        Self { conditions }
    }
}

/// Everything a decision depends on besides the note itself.
#[derive(Debug, Clone)]
pub struct Context<'a> {
    pub snapshot: &'a Snapshot,
    pub ledger: Option<&'a BudgetLedger>,
    /// Loss events the deferred queue already reserved for a resumed entry.
    pub reserved: Option<&'a [LossEvent]>,
    pub query: ReadQuery,
    pub now: Timestamp,
    /// Informational `adoption.by` id, not an attestation.
    pub by: String,
}

impl<'a> Context<'a> {
    pub fn new(snapshot: &'a Snapshot, now: Timestamp) -> Self {
        Self {
            snapshot,
            ledger: None,
            reserved: None,
            query: ReadQuery::default(),
            now,
            by: "wi-lifecycle".into(),
        }
    }

    pub fn with_ledger(self, ledger: &'a BudgetLedger) -> Self {
        Self {
            ledger: Some(ledger),
            ..self
        }
    }

    pub fn with_reserved(self, reserved: &'a [LossEvent]) -> Self {
        Self {
            reserved: Some(reserved),
            ..self
        }
    }

    pub fn with_query(self, query: ReadQuery) -> Self {
        Self { query, ..self }
    }
}
