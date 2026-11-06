//! Lifecycle-aware mutations: writes and edits followed by adoption, MCP
//! claim edits as superseding candidates, reverts as `restores` forks, and
//! archive or move into the archive charged against the change cap.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use wi_history::{History, hash_bytes};
use wi_lifecycle::{
    ActiveCounts, Actor, BudgetExhausted, Context, DeferredEntry, LossAction, LossDecision,
    LossKind, NoteSnapshot, WorkItem,
};
use wi_note::{CLAIM_CONTENT_FIELDS, Note, OriginKind, generate_permalink, set_key};

use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::lifecycle::adopt::{merge, requests_of};
use crate::lifecycle::{Charge, actor_of};
use crate::ops::{EditRequest, MoveRequest, WriteRequest, apply_body_edit};
use crate::vault::{Ingress, Vault, WriteResult};

/// Attempts of a capped archive or move when the lock refuses a charge.
const LOSS_RETRIES: usize = 3;

/// The engine's decision on a visibility loss.
pub(crate) enum LossOutcome {
    /// Allowed; charge the change cap when `Some`.
    Allowed(Option<Charge>),
    /// Never allowed for this actor (rule or preference retirement).
    Refused(String),
    /// Capped: the queue entry and its work item.
    Deferred {
        exhausted: BudgetExhausted,
        work: Box<WorkItem>,
    },
}

fn is_lifecycle_bytes(bytes: &[u8]) -> bool {
    NoteSnapshot::from_bytes("x.md", bytes)
        .lifecycle
        .lifecycle_type
        != wi_note::LifecycleType::None
}

fn is_memory_bytes(bytes: &[u8]) -> bool {
    NoteSnapshot::from_bytes("x.md", bytes).is_memory()
}

/// `<dir>/<stem>-<tag>-<8 hex>.md`, a sibling path for a new revision.
fn sibling_path(path: &str, tag: &str, seed: &[u8]) -> String {
    let stem = path.strip_suffix(".md").unwrap_or(path);
    let hash = hash_bytes(seed);
    let short = &hash["sha256:".len().."sha256:".len() + 8];
    format!("{stem}-{tag}-{short}.md")
}

impl Vault {
    /// `write` followed by automatic adoption of a lifecycle candidate.
    pub fn lifecycle_write(&self, request: WriteRequest) -> Result<WriteResult> {
        let mut result = self.write(request)?;
        if !result.replayed {
            self.adopt_after_write(&mut result)?;
        }
        Ok(result)
    }

    /// `edit` with lifecycle rules: an MCP change to claim content of a
    /// memory note creates a superseding candidate (the note itself is never
    /// changed in place); any other edit is followed by adoption.
    pub fn lifecycle_edit(&self, request: EditRequest) -> Result<WriteResult> {
        match self.edit(request.clone()) {
            Err(StoreError::ClaimEditRequiresCandidate { path, fields })
                if request.ingress == Ingress::Mcp && self.lifecycle_on() =>
            {
                self.claim_revision(&request, &path, &fields)
            }
            Ok(mut result) => {
                if !result.replayed {
                    self.adopt_after_write(&mut result)?;
                }
                Ok(result)
            }
            Err(e) => Err(e),
        }
    }

    /// `edit_semantic` followed by adoption.
    pub fn lifecycle_edit_semantic(
        &self,
        request: EditRequest,
        identity: &Value,
    ) -> Result<WriteResult> {
        match self.edit_semantic(request.clone(), identity) {
            Err(StoreError::ClaimEditRequiresCandidate { path, fields })
                if request.ingress == Ingress::Mcp && self.lifecycle_on() =>
            {
                self.claim_revision(&request, &path, &fields)
            }
            Ok(mut result) => {
                if !result.replayed {
                    self.adopt_after_write(&mut result)?;
                }
                Ok(result)
            }
            Err(e) => Err(e),
        }
    }

    /// The new candidate note for an MCP claim edit (KNOWLEDGE "Claim
    /// revisions"). Only claim-content keys and the body may
    /// change in such a request.
    fn claim_revision(
        &self,
        request: &EditRequest,
        path: &str,
        fields: &[String],
    ) -> Result<WriteResult> {
        let refuse = |message: &str| StoreError::LifecycleRefused {
            code: "claim-edit-mixed",
            message: message.into(),
        };
        if !request.remove.is_empty() {
            return Err(refuse(
                "a claim revision changes claim fields and the body only; send removals separately",
            ));
        }
        if request
            .set
            .iter()
            .any(|(k, _)| !CLAIM_CONTENT_FIELDS.contains(&k.as_str()))
        {
            return Err(refuse(
                "a claim revision changes claim fields only; send other keys separately",
            ));
        }
        let bytes = read_rel(self.root(), path)?.ok_or_else(|| StoreError::NotFound {
            path: path.to_owned(),
        })?;
        let current = hash_bytes(&bytes);
        if let Some(expected) = &request.expected_checksum
            && expected != &current
        {
            return Err(StoreError::Stale {
                path: path.to_owned(),
                expected: expected.clone(),
                current: Some(current),
            });
        }
        let old = Note::parse(&bytes)?;
        if !is_memory_bytes(&bytes) {
            return Err(StoreError::ClaimEditRequiresCandidate {
                path: path.to_owned(),
                fields: fields.to_vec(),
            });
        }
        let changes: BTreeMap<String, Value> = request.set.iter().cloned().collect();
        let base = match &request.body {
            Some(edit) => apply_body_edit(&old, edit)?,
            None => old.clone(),
        };
        let now = self.lifecycle_now();
        let revised = self
            .lifecycle_engine()
            .mcp_revision(&base, path, &changes, now)
            .map_err(|e| StoreError::LifecycleRefused {
                code: "claim-edit-refused",
                message: e.to_string(),
            })?;
        let seed = format!(
            "{path}\0{}\0{}\0{}",
            serde_json::to_string(&changes).unwrap_or_default(),
            serde_json::to_string(&request.body).unwrap_or_default(),
            current
        );
        let new_path = sibling_path(path, "rev", seed.as_bytes());
        let revised = self.own_permalink(revised, &new_path)?;
        let mut result = self.publish_engine_bytes(
            &new_path,
            revised.to_bytes().to_vec(),
            None,
            Ingress::Mcp,
            None,
        )?;
        let report = self.adopt(&new_path)?;
        result.lifecycle = Some(json!({
            "claim_edit": {
                "original": path,
                "candidate": new_path,
                "supersedes": [path],
                "fields": fields,
                "outcome": report.outcome,
            },
            "adoption": report,
        }));
        Ok(result)
    }

    /// A new revision must not reuse the explicit permalink of the note it
    /// copies; it gets the permalink of its own path.
    fn own_permalink(&self, note: Note, path: &str) -> Result<Note> {
        let has = note
            .frontmatter()
            .and_then(|f| f.get("permalink"))
            .is_some();
        if !has {
            return Ok(note);
        }
        set_key(&note, "permalink", &Value::String(generate_permalink(path)))
            .map_err(|e| StoreError::Edit(e.to_string()))
    }

    /// Revert: a lifecycle note (now or in the restored bytes) is restored
    /// as a new note naming the earlier revision in `restores`, which then
    /// passes the gates; plain notes are restored in place.
    pub fn lifecycle_revert(
        &self,
        path: &str,
        commit: &str,
        expected_checksum: Option<String>,
        op_id: Option<String>,
        ingress: Ingress,
    ) -> Result<WriteResult> {
        let rel = crate::paths::normalize(self.root(), path)?;
        if ingress != Ingress::Engine {
            crate::paths::check_engine_dir(&self.config, &rel)?;
        }
        let restored = History::read_at(&self.history, commit, &rel)?;
        let current = read_rel(self.root(), &rel)?;
        let is_work = |b: &[u8]| {
            NoteSnapshot::from_bytes("x.md", b).lifecycle.lifecycle_type
                == wi_note::LifecycleType::Work
        };
        if ingress != Ingress::Engine
            && (is_work(&restored) || current.as_deref().is_some_and(is_work))
        {
            return Err(StoreError::EngineOwnedField {
                field: "type".into(),
                ingress: ingress.actor(),
            });
        }
        let lifecycle =
            is_lifecycle_bytes(&restored) || current.as_deref().is_some_and(is_lifecycle_bytes);
        if !lifecycle || !self.lifecycle_on() {
            return self.revert_with(&rel, commit, expected_checksum, op_id, ingress);
        }
        if let Some(expected) = &expected_checksum {
            let now = current.as_deref().map(hash_bytes);
            if now.as_ref() != Some(expected) {
                return Err(StoreError::Stale {
                    path: rel,
                    expected: expected.clone(),
                    current: now,
                });
            }
        }
        let origin = match ingress {
            Ingress::Mcp => OriginKind::Agent,
            Ingress::Import => OriginKind::File,
            _ => OriginKind::User,
        };
        let old = Note::parse(&restored)?;
        let fork = self
            .lifecycle_engine()
            .restore_revision(&old, &rel, Some(commit), origin, self.lifecycle_now())
            .map_err(|e| StoreError::LifecycleRefused {
                code: "revert-refused",
                message: e.to_string(),
            })?;
        let new_path = sibling_path(&rel, "restore", format!("{rel}\0{commit}").as_bytes());
        let fork = self.own_permalink(fork, &new_path)?;
        if let Some(existing) = read_rel(self.root(), &new_path)? {
            return Err(StoreError::Exists {
                path: new_path,
                current: hash_bytes(&existing),
            });
        }
        let mut result =
            self.publish_engine_bytes(&new_path, fork.to_bytes().to_vec(), None, ingress, None)?;
        let report = self.adopt(&new_path)?;
        result.lifecycle = Some(json!({
            "revert_fork": {"restores": {"ref": rel, "commit": commit}, "note": new_path},
            "adoption": report,
        }));
        Ok(result)
    }

    /// The engine's loss decision for `actor` on `path` (archive, or move
    /// into the archive), against the live vault.
    pub(crate) fn loss_outcome(
        &self,
        path: &str,
        kind: LossKind,
        actor: Actor,
    ) -> Result<LossOutcome> {
        if !self.lifecycle_on() {
            return Ok(LossOutcome::Allowed(None));
        }
        let Some((target, _)) = self.note_snapshot(path)? else {
            return Ok(LossOutcome::Allowed(None));
        };
        if !target.is_memory() {
            return Ok(LossOutcome::Allowed(None));
        }
        let snapshot = self.decision_snapshot(std::slice::from_ref(&target), &[path])?;
        let ledger = self.loss_ledger()?;
        let ctx = Context::new(&snapshot, self.lifecycle_now()).with_ledger(&ledger);
        let engine = self.lifecycle_engine();
        let action = LossAction {
            kind,
            target: path.to_owned(),
            actor,
        };
        Ok(match engine.authorize_loss(&action, &ctx) {
            LossDecision::Allowed { losses, .. } if losses.is_empty() => LossOutcome::Allowed(None),
            LossDecision::Allowed { losses, .. } => LossOutcome::Allowed(Some(Charge {
                requests: requests_of(&losses),
                actor,
                counts: ActiveCounts::from_snapshot(&snapshot, &engine.policy),
                events: losses,
            })),
            LossDecision::Refused { reason } => LossOutcome::Refused(reason),
            LossDecision::Deferred {
                exhausted, work, ..
            } => LossOutcome::Deferred { exhausted, work },
        })
    }

    /// Authorize a visibility loss of `path` for `ingress`. Returns the
    /// charge for the move; a capped change is queued (its work note
    /// records the target's hash) and refused with `budget-exhausted`.
    fn authorize_archive(
        &self,
        path: &str,
        kind: LossKind,
        ingress: Ingress,
    ) -> Result<Option<Charge>> {
        match self.loss_outcome(path, kind, actor_of(ingress))? {
            LossOutcome::Allowed(charge) => Ok(charge),
            LossOutcome::Refused(reason) => Err(StoreError::LifecycleRefused {
                code: "rule-retirement",
                message: reason,
            }),
            LossOutcome::Deferred { exhausted, work } => {
                let hash = read_rel(self.root(), path)?.map(|b| hash_bytes(&b));
                let work = WorkItem {
                    deferred: work.deferred.map(|entry| DeferredEntry {
                        target_hash: hash,
                        ..entry
                    }),
                    ..*work
                };
                let work = self.write_work(&work)?;
                Err(StoreError::BudgetExhausted {
                    retry_at: exhausted.retry_at.map(wi_lifecycle::format_time),
                    spent: exhausted.spent.len(),
                    work: Some(work),
                })
            }
        }
    }

    /// Authorize, then move charged. A refusal under the publisher lock
    /// (another process spent the budget after the decision) decides again,
    /// so a capped change is always queued before `budget-exhausted` is
    /// reported.
    fn archive_move(&self, request: MoveRequest, kind: LossKind) -> Result<WriteResult> {
        let from = crate::paths::normalize(self.root(), &request.from)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let charge = self.authorize_archive(&from, kind, request.ingress)?;
            match self.move_charged(request.clone(), charge.clone()) {
                Err(StoreError::BudgetExhausted { work: None, .. }) if attempt < LOSS_RETRIES => {}
                Err(StoreError::BudgetExhausted {
                    retry_at,
                    spent,
                    work: None,
                }) => {
                    return Err(StoreError::BudgetExhausted {
                        retry_at,
                        spent,
                        work: self.queue_loss(&from, kind, request.ingress)?,
                    });
                }
                Ok(mut result) => {
                    self.after_archive(&mut result, charge)?;
                    return Ok(result);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Queue a loss the lock refused although the decision allowed it.
    fn queue_loss(&self, path: &str, kind: LossKind, ingress: Ingress) -> Result<Option<String>> {
        match self.authorize_archive(path, kind, ingress) {
            Err(StoreError::BudgetExhausted { work, .. }) => Ok(work),
            Err(e) => Err(e),
            Ok(_) => Ok(None),
        }
    }

    /// Archive with the lifecycle loss rules.
    pub fn lifecycle_archive(
        &self,
        path: &str,
        expected_checksum: Option<String>,
        op_id: Option<String>,
        ingress: Ingress,
    ) -> Result<WriteResult> {
        let rel = crate::paths::normalize(self.root(), path)?;
        let destination = self.archive_destination(&rel)?;
        self.archive_move(
            MoveRequest {
                from: rel,
                to: destination,
                expected_checksum,
                op_id,
                ingress,
            },
            LossKind::Archive,
        )
    }

    /// A deferred archive resumed by the queue, re-authorized by the caller.
    pub(crate) fn archive_charged(
        &self,
        path: &str,
        charge: Option<Charge>,
    ) -> Result<WriteResult> {
        let destination = self.archive_destination(path)?;
        let mut result = self.move_charged(
            MoveRequest {
                from: path.to_owned(),
                to: destination,
                expected_checksum: None,
                op_id: None,
                ingress: Ingress::Engine,
            },
            charge.clone(),
        )?;
        self.after_archive(&mut result, charge)?;
        Ok(result)
    }

    /// Move with the lifecycle loss rules: a move into the archive is
    /// charged like an archive.
    pub fn lifecycle_move(&self, request: MoveRequest) -> Result<WriteResult> {
        let from = crate::paths::normalize(self.root(), &request.from)?;
        let to = crate::paths::normalize(self.root(), &request.to)?;
        let into_archive = self.is_archived_path(&to) && !self.is_archived_path(&from);
        if !into_archive {
            return self.move_note(request);
        }
        self.archive_move(request, LossKind::MoveToArchive)
    }

    /// After a memory note moved under the archive: record `status:
    /// archived` (the path already makes it not current) and report.
    fn after_archive(&self, result: &mut WriteResult, charge: Option<Charge>) -> Result<()> {
        if !self.lifecycle_on() {
            return Ok(());
        }
        let Some((snap, _)) = self.note_snapshot(&result.path)? else {
            return Ok(());
        };
        if !snap.is_memory() {
            return Ok(());
        }
        let edit = wi_lifecycle::NoteEdit::new(&result.path).with("status", json!("archived"));
        if let Err(e) = self.apply_edit(&edit, None, None) {
            // Locked or read-only frontmatter: the archive path already
            // withholds the note; report instead of failing the move.
            result.lifecycle = Some(json!({"archive_status_error": e.code()}));
            return Ok(());
        }
        let charged = charge.map_or(0, |c| c.events.len());
        result.lifecycle = Some(merge(
            result.lifecycle.take(),
            json!({"archived": true, "losses_charged": charged}),
        ));
        Ok(())
    }

    fn archive_destination(&self, rel: &str) -> Result<String> {
        let dir = self.config.archive_dir().trim_matches('/');
        if rel.starts_with(&format!("{dir}/")) {
            return Err(StoreError::Invalid(format!("{rel} is already archived")));
        }
        Ok(format!("{dir}/{rel}"))
    }
}
