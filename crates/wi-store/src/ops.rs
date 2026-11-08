//! Write, edit, move, archive and revert requests: preconditions and
//! engine-owned field rules (FORMAT, AUTHORITY), then the write path.

use serde::Serialize;
use serde_json::{Value, json};
use wi_history::{History, hash_bytes};
use wi_lifecycle::LIFECYCLE_TYPES;
use wi_note::{
    CLAIM_CONTENT_FIELDS, ENGINE_OWNED_FIELDS, Note, generate_permalink, remove_key, set_key,
};

use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::paths::{Target, check_engine_dir, is_markdown, resolve_writable_as};
use crate::publish::{Change, Expect, request_digest};
use crate::vault::{Ingress, Vault, WriteResult};

/// Create a note, or replace one when `expected_checksum` is given.
#[derive(Debug, Clone)]
pub struct WriteRequest {
    pub path: String,
    pub content: Vec<u8>,
    pub expected_checksum: Option<String>,
    pub op_id: Option<String>,
    pub ingress: Ingress,
}

/// A bounded body edit command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum BodyEdit {
    Append {
        content: String,
    },
    Prepend {
        content: String,
    },
    FindReplace {
        find: String,
        replace: String,
        expected_replacements: usize,
    },
}

/// Byte-preserving frontmatter changes plus an optional body edit.
#[derive(Debug, Clone)]
pub struct EditRequest {
    pub path: String,
    pub set: Vec<(String, Value)>,
    pub remove: Vec<String>,
    pub body: Option<BodyEdit>,
    pub expected_checksum: Option<String>,
    pub op_id: Option<String>,
    pub ingress: Ingress,
}

/// Move or rename a note; the permalink is kept.
#[derive(Debug, Clone)]
pub struct MoveRequest {
    pub from: String,
    pub to: String,
    pub expected_checksum: Option<String>,
    pub op_id: Option<String>,
    pub ingress: Ingress,
}

const EDIT_RETRIES: usize = 3;

fn note_type(note: &Note) -> Option<String> {
    note.frontmatter()
        .and_then(|f| f.get("type"))
        .and_then(|v| v.as_str().map(str::to_owned))
}

fn is_lifecycle(note: &Note) -> bool {
    note_type(note).is_some_and(|t| LIFECYCLE_TYPES.contains(&t.as_str()))
}

fn field(note: &Note, key: &str) -> Option<Value> {
    note.frontmatter().and_then(|f| f.get(key))
}

/// `locked: true` (YAML boolean) opts a note out of engine edits.
pub(crate) fn is_locked(note: &Note) -> bool {
    field(note, "locked") == Some(Value::Bool(true))
}

pub(crate) fn has_markers(bytes: &[u8]) -> bool {
    wi_index::has_conflict_markers(&String::from_utf8_lossy(bytes))
}

fn body(note: &Note) -> &[u8] {
    &note.to_bytes()[note.body_start()..]
}

fn is_memory(note: &Note) -> bool {
    note_type(note).as_deref() == Some("memory")
}

/// Claim-content keys whose value differs between two versions. For a
/// memory note (before or after) the Markdown body is claim content too
/// and is reported as `body`.
pub(crate) fn changed_claim_fields(old: &Note, new: &Note) -> Vec<String> {
    let mut fields: Vec<String> = CLAIM_CONTENT_FIELDS
        .iter()
        .filter(|k| field(old, k) != field(new, k))
        .map(|k| (*k).to_owned())
        .collect();
    if (is_memory(old) || is_memory(new)) && body(old) != body(new) {
        fields.push("body".into());
    }
    fields
}

/// Work notes are engine-owned executable state: no ordinary
/// ingress may create one, change one or turn a note into one.
fn refuse_work_type(old: Option<&Note>, new: &Note, ingress: Ingress) -> Result<()> {
    let work = |n: &Note| note_type(n).as_deref() == Some("work");
    if ingress != Ingress::Engine && (work(new) || old.is_some_and(work)) {
        return Err(StoreError::EngineOwnedField {
            field: "type".into(),
            ingress: ingress.actor(),
        });
    }
    Ok(())
}

/// The agent label of a noted rule or preference. Its removal means owner
/// adoption (AUTHORITY), so an agent may not remove or change it, and an
/// ordinary rewrite that omits it keeps it. The owner removes it in a file
/// editor or with `wi adopt`.
const AGENT_LABEL: &str = "noted_by";

/// Fields a non-engine write may not change: the engine-owned fields, and
/// for MCP also the agent label.
fn protected_fields(ingress: Ingress) -> impl Iterator<Item = &'static str> {
    ENGINE_OWNED_FIELDS
        .iter()
        .copied()
        .chain((ingress == Ingress::Mcp).then_some(AGENT_LABEL))
}

fn refuse_engine_owned(key: &str, ingress: Ingress) -> Result<()> {
    if ingress != Ingress::Engine && protected_fields(ingress).any(|k| k == key) {
        return Err(StoreError::EngineOwnedField {
            field: key.to_owned(),
            ingress: ingress.actor(),
        });
    }
    Ok(())
}

/// The bytes a moved note gets at its new path: the old permalink becomes
/// explicit when the frontmatter has none, so the address survives.
pub(crate) fn move_bytes(bytes: &[u8], from: &str) -> Result<Vec<u8>> {
    let note = Note::parse(bytes)?;
    if field(&note, "permalink").is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
        return Ok(bytes.to_vec());
    }
    let editable = note.frontmatter().is_none_or(|f| f.is_editable().is_ok());
    if !editable {
        return Ok(bytes.to_vec());
    }
    let permalink = Value::String(generate_permalink(from));
    let edited =
        set_key(&note, "permalink", &permalink).map_err(|e| StoreError::Edit(e.to_string()))?;
    Ok(edited.to_bytes().to_vec())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl Vault {
    fn note_target(&self, path: &str, ingress: Ingress) -> Result<Target> {
        let managed = ingress == Ingress::Engine || self.managed_write.get();
        let target = match self.import_target(path) {
            Some(target) => target?,
            None => resolve_writable_as(self.root(), &self.config, path, managed)?,
        };
        if ingress != Ingress::Engine {
            check_engine_dir(&self.config, &target.rel)?;
        }
        if !is_markdown(&target.rel) {
            return Err(StoreError::Invalid(format!(
                "{} is not a Markdown note (.md); import attachments instead",
                target.rel
            )));
        }
        Ok(target)
    }

    /// Read the current note for an engine change: refuse locked notes and
    /// files with conflict markers.
    fn current_for_change(&self, target: &Target) -> Result<Option<(Vec<u8>, Note)>> {
        self.held_gate(&[target.rel.as_str()])?;
        let Some(bytes) = read_rel(self.root(), &target.rel)? else {
            return Ok(None);
        };
        if has_markers(&bytes) {
            return Err(StoreError::ConflictMarkers {
                path: target.rel.clone(),
            });
        }
        let note = Note::parse(&bytes)?;
        if is_locked(&note) {
            return Err(StoreError::Locked {
                path: target.rel.clone(),
            });
        }
        Ok(Some((bytes, note)))
    }

    /// The import-record target of `path` while an import-record write is
    /// running and `path` names one; other paths resolve as usual.
    fn import_target(&self, path: &str) -> Option<Result<Target>> {
        let named = crate::paths::normalize(self.root(), path)
            .is_ok_and(|rel| crate::paths::is_under(&rel, crate::paths::IMPORTS_DIR));
        (self.import_record.get() && named)
            .then(|| crate::paths::resolve_import_record(self.root(), path))
    }

    fn require_import_record(&self, path: &str) -> Result<()> {
        crate::paths::resolve_import_record(self.root(), path).map(|_| ())
    }

    /// Create or replace an import record under the hidden
    /// `.wiki-island/imports/` folder (an importer's mapping between runs).
    /// Only that folder is accepted; ordinary tools still refuse it.
    pub fn write_import_record(&self, request: WriteRequest) -> Result<WriteResult> {
        self.require_import_record(&request.path)?;
        self.import_record.set(true);
        let written = self.write(request);
        self.import_record.set(false);
        written
    }

    /// Move an import record an older version kept among the notes into
    /// the hidden `.wiki-island/imports/` folder (checksum-guarded).
    pub fn move_import_record(&self, request: MoveRequest) -> Result<WriteResult> {
        self.require_import_record(&request.to)?;
        self.import_record.set(true);
        let moved = self.move_note(request);
        self.import_record.set(false);
        moved
    }

    /// Create or replace a note (`wi write`, `wi_write`). Without an
    /// expected checksum it only creates.
    pub fn write(&self, request: WriteRequest) -> Result<WriteResult> {
        let target = self.note_target(&request.path, request.ingress)?;
        let digest = request_digest(&json!({
            "tool": "write",
            "path": target.rel,
            "content": hash_bytes(&request.content),
            "expected_checksum": request.expected_checksum,
        }));
        if let Some(first) = self.replay_operation(request.op_id.as_deref(), &digest)? {
            return Ok(first);
        }
        let new = Note::parse(&request.content)?;
        let current = self.current_for_change(&target)?;
        refuse_work_type(current.as_ref().map(|(_, n)| n), &new, request.ingress)?;
        // An agent's new claim: its rule or preference is kept as a plain
        // noted note; any other memory claim is completed (source refs
        // resolved, classification defaults filled).
        let mut noted = None;
        let (target, new, current) =
            if request.ingress == Ingress::Mcp && current.is_none() && self.lifecycle_on() {
                match crate::lifecycle::rule_kind(&new) {
                    Some(kind) => {
                        let (path, bytes) = self.noted_note(&target.rel, &new, kind)?;
                        let noted_target = self.note_target(&path, request.ingress)?;
                        let current = self.current_for_change(&noted_target)?;
                        noted = Some((target.rel, kind));
                        (noted_target, Note::parse(&bytes)?, current)
                    }
                    None => {
                        let new = self.complete_agent_claim(&target.rel, new)?;
                        (target, new, current)
                    }
                }
            } else {
                (target, new, current)
            };
        self.validate_lifecycle_write(&target.rel, &new, request.ingress)?;
        let (bytes, expect) = match (&current, &request.expected_checksum) {
            (Some((bytes, _)), None) => {
                return Err(StoreError::Exists {
                    path: target.rel.clone(),
                    current: hash_bytes(bytes),
                });
            }
            (None, Some(expected)) => {
                return Err(StoreError::Stale {
                    path: target.rel.clone(),
                    expected: expected.clone(),
                    current: None,
                });
            }
            (Some((bytes, old)), Some(expected)) => {
                let hash = hash_bytes(bytes);
                if &hash != expected {
                    return Err(StoreError::Stale {
                        path: target.rel.clone(),
                        expected: expected.clone(),
                        current: Some(hash),
                    });
                }
                let new = carry_engine_fields(old, &new, request.ingress)?;
                self.check_replace(old, &new, request.ingress)?;
                let new = self.apply_provenance(&target, old, new, request.ingress, "write")?;
                (new.to_bytes().to_vec(), Expect::Hash(hash))
            }
            (None, None) => (self.creation_bytes(&new, request.ingress)?, Expect::Absent),
        };
        let mut result = self.publish(Change {
            target,
            bytes,
            expect,
            moved_from: None,
            op_id: request.op_id,
            digest,
            ingress: request.ingress,
            revert_of: None,
            charge: None,
        })?;
        if let Some((requested, kind)) = noted {
            result.lifecycle = Some(json!({"adoption": {
                "path": result.path,
                "outcome": "noted",
                "kind": kind,
                "requested": requested,
            }}));
        }
        Ok(result)
    }

    /// Publish the bytes of a non-Markdown attachment an importer copies
    /// into the vault (for example an image beside converted pages). Only
    /// [`Ingress::Import`] may call it. The path is contained like a note
    /// write: nothing outside the vault, in `.git`, the engine folders or
    /// another application's folder. Without `expected_checksum` it only
    /// creates; with one it replaces exactly those bytes. Markdown files go
    /// through [`Vault::write`].
    pub fn write_attachment(
        &self,
        path: &str,
        bytes: Vec<u8>,
        expected_checksum: Option<String>,
        ingress: Ingress,
    ) -> Result<WriteResult> {
        if ingress != Ingress::Import {
            return Err(StoreError::Invalid(
                "attachments are written only by an import".into(),
            ));
        }
        let target = resolve_writable_as(self.root(), &self.config, path, false)?;
        check_engine_dir(&self.config, &target.rel)?;
        if is_markdown(&target.rel) {
            return Err(StoreError::Invalid(format!(
                "{} is a Markdown note; write it as a note",
                target.rel
            )));
        }
        self.held_gate(&[target.rel.as_str()])?;
        let current = read_rel(self.root(), &target.rel)?.map(|b| hash_bytes(&b));
        let expect = match (current, expected_checksum) {
            (Some(current), None) => {
                return Err(StoreError::Exists {
                    path: target.rel.clone(),
                    current,
                });
            }
            (None, Some(expected)) => {
                return Err(StoreError::Stale {
                    path: target.rel.clone(),
                    expected,
                    current: None,
                });
            }
            (Some(current), Some(expected)) if current != expected => {
                return Err(StoreError::Stale {
                    path: target.rel.clone(),
                    expected,
                    current: Some(current),
                });
            }
            (Some(current), Some(_)) => Expect::Hash(current),
            (None, None) => Expect::Absent,
        };
        let digest = request_digest(&json!({
            "tool": "attachment",
            "path": target.rel,
            "content": hash_bytes(&bytes),
        }));
        self.publish(Change {
            target,
            bytes,
            expect,
            moved_from: None,
            op_id: None,
            digest,
            ingress,
            revert_of: None,
            charge: None,
        })
    }

    /// Engine-owned fields may not change through a replace.
    fn check_replace(&self, old: &Note, new: &Note, ingress: Ingress) -> Result<()> {
        if ingress == Ingress::Engine {
            return Ok(());
        }
        for key in protected_fields(ingress) {
            if field(old, key) != field(new, key) {
                refuse_engine_owned(key, ingress)?;
            }
        }
        Ok(())
    }

    /// Origin by ingress whenever claim content changes (FORMAT, AUTHORITY).
    /// Lifecycle status is judged on both the old and the new bytes, so a
    /// detour through a plain `type` cannot carry provenance:
    ///
    /// - MCP may not change claim content of a note that is a lifecycle note
    ///   before or after the change, nor change the type of a lifecycle note;
    /// - a change that makes a plain note a lifecycle note is a creation: its
    ///   `origin` comes from this ingress, never from the old bytes;
    /// - a CLI or file change of claim content sets `origin.kind=user` and
    ///   resets an active memory note to `candidate`.
    fn apply_provenance(
        &self,
        target: &Target,
        old: &Note,
        new: Note,
        ingress: Ingress,
        verb: &str,
    ) -> Result<Note> {
        // `lifecycle: off` is the plain baseline: no claim-edit refusal, no
        // terminal protection, no provenance or status rewrite.
        if ingress == Ingress::Engine || !self.lifecycle_on() {
            return Ok(new);
        }
        let (old_lc, new_lc) = (is_lifecycle(old), is_lifecycle(&new));
        let claims = changed_claim_fields(old, &new);
        let type_changed = note_type(old) != note_type(&new);
        if old_lc
            && matches!(ingress, Ingress::Cli | Ingress::Watcher)
            && (!claims.is_empty() || type_changed)
            && field(old, "status")
                .and_then(|v| v.as_str().map(str::to_owned))
                .is_some_and(|s| matches!(s.as_str(), "superseded" | "rejected"))
        {
            return Err(StoreError::TerminalNote {
                path: target.rel.clone(),
            });
        }
        if ingress == Ingress::Mcp && (old_lc || new_lc) {
            let mut fields = claims.clone();
            if type_changed && old_lc {
                fields.push("type".into());
            }
            if !fields.is_empty() {
                return Err(StoreError::ClaimEditRequiresCandidate {
                    path: target.rel.clone(),
                    fields,
                });
            }
        }
        let promoted = new_lc && !old_lc;
        if !new_lc || (!promoted && claims.is_empty() && !type_changed) {
            return Ok(new);
        }
        let Some(kind) = ingress.origin_kind() else {
            return Ok(new);
        };
        let edit = |note: &Note, key: &str, value: &Value| {
            set_key(note, key, value)
                .map_err(|e| StoreError::Edit(format!("cannot record {key} for this change: {e}")))
        };
        let mut next = new;
        let current_kind = field(&next, "origin")
            .and_then(|o| o.get("kind").and_then(|k| k.as_str().map(str::to_owned)));
        if promoted || current_kind.as_deref() != Some(kind) {
            next = edit(&next, "origin", &origin_value(ingress, kind, verb))?;
        }
        let status = field(&next, "status").and_then(|v| v.as_str().map(str::to_owned));
        if note_type(&next).as_deref() == Some("memory") && status.is_some_and(|s| s != "candidate")
        {
            next = edit(&next, "status", &json!("candidate"))?;
        }
        Ok(next)
    }

    /// Refuse a future or unparseable timestamp in a lifecycle note before
    /// it is published (KNOWLEDGE Freshness, FORMAT). Inert with
    /// `lifecycle: off` and for engine writes.
    fn validate_lifecycle_write(&self, rel: &str, note: &Note, ingress: Ingress) -> Result<()> {
        if ingress == Ingress::Engine || !is_lifecycle(note) {
            return Ok(());
        }
        let snap = wi_lifecycle::NoteSnapshot::from_bytes(rel, note.to_bytes());
        let evidence = wi_lifecycle::Evidence::default();
        match self
            .lifecycle_engine()
            .validate_write(&snap, &evidence, self.lifecycle_now())
        {
            Ok(()) => Ok(()),
            Err(state) => Err(StoreError::LifecycleRefused {
                code: match state {
                    wi_lifecycle::FreshnessState::FutureTimestamp(_) => "future-timestamp",
                    _ => "unparseable-timestamp",
                },
                message: format!("{rel}: timestamp refused on write: {state:?}"),
            }),
        }
    }

    /// New-note bytes: engine-owned fields are refused (except `restores`)
    /// and `origin` is recorded from the ingress.
    fn creation_bytes(&self, note: &Note, ingress: Ingress) -> Result<Vec<u8>> {
        if ingress == Ingress::Engine {
            return Ok(note.to_bytes().to_vec());
        }
        for key in ENGINE_OWNED_FIELDS {
            if *key != "restores" && field(note, key).is_some() {
                refuse_engine_owned(key, ingress)?;
            }
        }
        // Document provenance is the validated source record. Keep its strict
        // document frontmatter schema free of an extra origin field.
        if ingress == Ingress::Import
            && note_type(note)
                .is_some_and(|t| matches!(t.as_str(), "extracted_text" | "document_ingestion_run"))
        {
            return Ok(note.to_bytes().to_vec());
        }
        let Some(kind) = ingress.origin_kind() else {
            return Ok(note.to_bytes().to_vec());
        };
        if note.frontmatter().is_some_and(|f| f.is_editable().is_err()) {
            if is_lifecycle(note) {
                return Err(StoreError::Edit(
                    "the frontmatter cannot be edited, so origin cannot be recorded".into(),
                ));
            }
            return Ok(note.to_bytes().to_vec());
        }
        let origin = origin_value(ingress, kind, "write");
        let edited =
            set_key(note, "origin", &origin).map_err(|e| StoreError::Edit(e.to_string()))?;
        Ok(edited.to_bytes().to_vec())
    }

    /// Byte-preserving frontmatter edit plus optional body edit
    /// (`wi edit`, `wi_edit`).
    pub fn edit(&self, request: EditRequest) -> Result<WriteResult> {
        self.edit_inner(request, None)
    }

    /// Replay an MCP edit whose textual patch was prepared from a section
    /// selector. The identity must contain every original tool argument.
    pub fn replay_semantic_edit(
        &self,
        op_id: Option<&str>,
        identity: &Value,
    ) -> Result<Option<WriteResult>> {
        let digest = request_digest(&json!({"tool":"mcp-edit", "request":identity}));
        self.replay_operation(op_id, &digest)
    }

    /// Publish a prepared, checksum-pinned section patch using the original
    /// semantic request for idempotency, rather than a changing derived patch.
    pub fn edit_semantic(&self, request: EditRequest, identity: &Value) -> Result<WriteResult> {
        self.edit_inner(request, Some(identity))
    }

    fn edit_inner(&self, request: EditRequest, identity: Option<&Value>) -> Result<WriteResult> {
        let target = self.note_target(&request.path, request.ingress)?;
        let digest = request_digest(&json!({
            "tool": "edit",
            "path": target.rel,
            "set": request.set,
            "remove": request.remove,
            "body": request.body,
            "expected_checksum": request.expected_checksum,
        }));
        let digest = identity.map_or(digest, |identity| {
            request_digest(&json!({"tool":"mcp-edit", "request":identity}))
        });
        if let Some(first) = self.replay_operation(request.op_id.as_deref(), &digest)? {
            return Ok(first);
        }
        for key in request.set.iter().map(|(k, _)| k).chain(&request.remove) {
            refuse_engine_owned(key, request.ingress)?;
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (bytes, note) =
                self.current_for_change(&target)?
                    .ok_or_else(|| StoreError::NotFound {
                        path: target.rel.clone(),
                    })?;
            let hash = hash_bytes(&bytes);
            if let Some(expected) = &request.expected_checksum
                && expected != &hash
            {
                return Err(StoreError::Stale {
                    path: target.rel.clone(),
                    expected: expected.clone(),
                    current: Some(hash),
                });
            }
            let edited = apply_edit(&note, &request)?;
            refuse_work_type(Some(&note), &edited, request.ingress)?;
            self.validate_lifecycle_write(&target.rel, &edited, request.ingress)?;
            let edited = self.apply_provenance(&target, &note, edited, request.ingress, "edit")?;
            let result = self.publish(Change {
                target: target.clone(),
                bytes: edited.to_bytes().to_vec(),
                expect: Expect::Hash(hash),
                moved_from: None,
                op_id: request.op_id.clone(),
                digest: digest.clone(),
                ingress: request.ingress,
                revert_of: None,
                charge: None,
            });
            match result {
                // A concurrent change between our read and the lock: redo
                // the edit on the new bytes unless the caller pinned them.
                Err(StoreError::Stale { .. })
                    if request.expected_checksum.is_none() && attempt < EDIT_RETRIES => {}
                other => return other,
            }
        }
    }

    /// Move a note within the vault (`wi move`, `wi_move`).
    pub fn move_note(&self, request: MoveRequest) -> Result<WriteResult> {
        self.move_charged(request, None)
    }

    /// A move that charges the change cap under the publisher lock.
    pub(crate) fn move_charged(
        &self,
        request: MoveRequest,
        charge: Option<crate::lifecycle::Charge>,
    ) -> Result<WriteResult> {
        let managed = request.ingress == Ingress::Engine;
        let source = resolve_writable_as(self.root(), &self.config, &request.from, managed)?;
        let target = match self.import_target(&request.to) {
            Some(target) => target?,
            None => resolve_writable_as(self.root(), &self.config, &request.to, managed)?,
        };
        if request.ingress != Ingress::Engine {
            check_engine_dir(&self.config, &source.rel)?;
            check_engine_dir(&self.config, &target.rel)?;
        }
        if is_markdown(&source.rel) != is_markdown(&target.rel) {
            return Err(StoreError::Invalid(
                "A move must preserve the file kind.".into(),
            ));
        }
        if source.rel == target.rel {
            return Err(StoreError::Invalid("source and target are the same".into()));
        }
        let digest = request_digest(&json!({
            "tool": "move",
            "from": source.rel,
            "to": target.rel,
            "expected_checksum": request.expected_checksum,
        }));
        if let Some(first) = self.replay_operation(request.op_id.as_deref(), &digest)? {
            return Ok(first);
        }
        self.held_gate(&[source.rel.as_str(), target.rel.as_str()])?;
        let bytes = if is_markdown(&source.rel) {
            self.current_for_change(&source)?.map(|(bytes, _)| bytes)
        } else {
            read_rel(self.root(), &source.rel)?
        }
        .ok_or_else(|| StoreError::NotFound {
            path: source.rel.clone(),
        })?;
        let hash = hash_bytes(&bytes);
        if let Some(expected) = &request.expected_checksum
            && expected != &hash
        {
            return Err(StoreError::Stale {
                path: source.rel.clone(),
                expected: expected.clone(),
                current: Some(hash),
            });
        }
        if let Some(existing) = read_rel(self.root(), &target.rel)? {
            return Err(StoreError::Exists {
                path: target.rel.clone(),
                current: hash_bytes(&existing),
            });
        }
        let moved = if is_markdown(&source.rel) {
            move_bytes(&bytes, &source.rel)?
        } else {
            bytes
        };
        self.publish(Change {
            target,
            bytes: moved,
            expect: Expect::Absent,
            moved_from: Some((source, hash)),
            op_id: request.op_id,
            digest,
            ingress: request.ingress,
            revert_of: None,
            charge,
        })
    }

    /// Archive a note: move it under the archive directory, keeping its
    /// relative path and permalink. There is no delete.
    pub fn archive(
        &self,
        path: &str,
        expected_checksum: Option<String>,
        op_id: Option<String>,
        ingress: Ingress,
    ) -> Result<WriteResult> {
        let source = self.note_target(path, ingress)?;
        let dir = self.config.archive_dir().trim_matches('/');
        if source.rel.starts_with(&format!("{dir}/")) {
            return Err(StoreError::Invalid(format!(
                "{} is already archived",
                source.rel
            )));
        }
        self.move_note(MoveRequest {
            from: source.rel.clone(),
            to: format!("{dir}/{}", source.rel),
            expected_checksum,
            op_id,
            ingress,
        })
    }

    /// Restore the bytes `path` had at `commit` as a new write (and so a
    /// new commit with `Wi-Revert`). History is never rewritten. A lifecycle
    /// note (now or in the restored bytes) is refused: its revert is a fork
    /// with `restores`. A missing file is restored through the
    /// creation checks, with `origin` from this ingress.
    pub fn revert(&self, path: &str, commit: &str, ingress: Ingress) -> Result<WriteResult> {
        self.revert_with(path, commit, None, None, ingress)
    }

    /// Revert with the same optional checksum and operation-id preconditions
    /// as other MCP writes. Idempotency is checked before reading old bytes.
    pub fn revert_with(
        &self,
        path: &str,
        commit: &str,
        expected_checksum: Option<String>,
        op_id: Option<String>,
        ingress: Ingress,
    ) -> Result<WriteResult> {
        let target = self.note_target(path, ingress)?;
        let digest = request_digest(
            &json!({"tool":"revert", "path":target.rel, "commit":commit, "expected_checksum":expected_checksum}),
        );
        if let Some(first) = self.replay_operation(op_id.as_deref(), &digest)? {
            return Ok(first);
        }
        let restored_bytes = History::read_at(&self.history, commit, &target.rel)?;
        let restored = Note::parse(&restored_bytes)?;
        let current = self.current_for_change(&target)?;
        if let Some(expected) = &expected_checksum {
            let current_hash = current.as_ref().map(|(bytes, _)| hash_bytes(bytes));
            if current_hash.as_ref() != Some(expected) {
                return Err(StoreError::Stale {
                    path: target.rel.clone(),
                    expected: expected.clone(),
                    current: current_hash,
                });
            }
        }
        if ingress != Ingress::Engine
            && (is_lifecycle(&restored) || current.as_ref().is_some_and(|(_, n)| is_lifecycle(n)))
        {
            return Err(StoreError::LifecycleRevertRequiresFork { path: target.rel });
        }
        let (bytes, expect) = match &current {
            Some((current, old)) => {
                self.check_replace(old, &restored, ingress)?;
                (restored_bytes, Expect::Hash(hash_bytes(current)))
            }
            None => {
                let restored = if ingress != Ingress::Engine && field(&restored, "origin").is_some()
                {
                    remove_key(&restored, "origin").map_err(|e| StoreError::Edit(e.to_string()))?
                } else {
                    restored
                };
                (self.creation_bytes(&restored, ingress)?, Expect::Absent)
            }
        };
        self.publish(Change {
            target,
            bytes,
            expect,
            moved_from: None,
            op_id,
            digest,
            ingress,
            revert_of: Some(commit.to_owned()),
            charge: None,
        })
    }
}

fn origin_value(ingress: Ingress, kind: &str, verb: &str) -> Value {
    json!({
        "kind": kind,
        "locator": format!("{}:{verb}", ingress.actor()),
        "captured_at": now_rfc3339(),
    })
}

/// A replace that omits an engine-owned key or the agent label keeps its
/// current value, so a tool can rewrite a note without restating `origin`;
/// a different value is still refused by `check_replace`.
fn carry_engine_fields(old: &Note, new: &Note, ingress: Ingress) -> Result<Note> {
    if ingress == Ingress::Engine {
        return Ok(new.clone());
    }
    let mut next = new.clone();
    for key in ENGINE_OWNED_FIELDS.iter().chain(&[AGENT_LABEL]) {
        if let (Some(value), None) = (field(old, key), field(new, key)) {
            next = set_key(&next, key, &value)
                .map_err(|e| StoreError::Edit(format!("cannot keep engine-owned {key}: {e}")))?;
        }
    }
    Ok(next)
}

fn apply_edit(note: &Note, request: &EditRequest) -> Result<Note> {
    let mut current = note.clone();
    for (key, value) in &request.set {
        current = set_key(&current, key, value).map_err(|e| StoreError::Edit(e.to_string()))?;
    }
    for key in &request.remove {
        current = remove_key(&current, key).map_err(|e| StoreError::Edit(e.to_string()))?;
    }
    if let Some(edit) = &request.body {
        current = apply_body_edit(&current, edit)?;
    }
    Ok(current)
}

pub(crate) fn apply_body_edit(note: &Note, edit: &BodyEdit) -> Result<Note> {
    let raw = note.to_bytes();
    let start = note.body_start();
    let (head, body) = raw.split_at(start);
    let body = std::str::from_utf8(body).map_err(wi_note::ParseError::from)?;
    let next = match edit {
        BodyEdit::Append { content } => {
            let mut text = body.to_owned();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(content);
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text
        }
        BodyEdit::Prepend { content } => {
            let mut text = content.clone();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(body);
            text
        }
        BodyEdit::FindReplace {
            find,
            replace,
            expected_replacements,
        } => {
            if find.is_empty() {
                return Err(StoreError::Invalid("find text is empty".into()));
            }
            let found = body.matches(find.as_str()).count();
            if found != *expected_replacements {
                return Err(StoreError::Edit(format!(
                    "found {found} occurrence(s) of the find text, expected {expected_replacements}"
                )));
            }
            body.replace(find.as_str(), replace)
        }
    };
    let mut bytes = head.to_vec();
    bytes.extend_from_slice(next.as_bytes());
    Ok(Note::parse(&bytes)?)
}
