//! Work items as `type: work` notes: listing, closing with an honest
//! outcome, opening conflict items and converting per-device work requests.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wi_lifecycle::{Outcome, Reopen, Snapshot, State, Trigger, WorkItem, WorkKind};
use wi_note::Note;

use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::vault::Vault;
use crate::work::{WorkKind as RequestKind, WorkRequest};

/// One work note, for `wi work list` and `wi_work`.
#[derive(Debug, Clone, Serialize)]
pub struct WorkEntry {
    pub path: String,
    pub work_id: Option<String>,
    pub work_kind: Option<String>,
    pub target: Option<String>,
    pub state: Option<String>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub next_action: Option<String>,
    pub deferred: bool,
    /// Only this device may act on the item.
    pub actionable: bool,
}

/// A close request: outcome, limits and the reopen trigger.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkClose {
    pub path: String,
    pub outcome: Outcome,
    #[serde(default)]
    pub limits: Option<String>,
    #[serde(default)]
    pub reopen_trigger: Option<Trigger>,
    #[serde(default)]
    pub reopen_detail: Option<String>,
}

fn text(note: &Note, key: &str) -> Option<String> {
    note.frontmatter()
        .and_then(|f| f.get(key))
        .and_then(|v| v.as_str().map(str::to_owned))
}

/// Rebuild the engine work item from a work note's frontmatter. Only a
/// `type: work` note is a work item.
fn work_item(note: &Note) -> Option<WorkItem> {
    let front = note.frontmatter()?;
    if front.get("type").as_ref().and_then(Value::as_str) != Some("work") {
        return None;
    }
    let work = front.get("work")?;
    // The condition fingerprint: persisted at creation (`work.condition`);
    // notes written before that carry it only in `reopen.observed`.
    let condition = work
        .get("condition")
        .filter(|v| !v.is_null())
        .or_else(|| work.get("reopen").and_then(|r| r.get("observed")))
        .cloned()
        .unwrap_or(Value::Null);
    let value = json!({
        "id": front.get("work_id")?,
        "kind": front.get("work_kind")?,
        "target": work.get("target")?,
        "related": front.get("related").unwrap_or_else(|| json!([])),
        "state": work.get("state")?,
        "reason": work.get("reason").cloned().unwrap_or(json!("")),
        "next_action": work.get("next_action").cloned().unwrap_or(json!("")),
        "next_trigger": work.get("next_trigger").cloned().unwrap_or(json!("session")),
        "outcome": work.get("outcome").cloned().unwrap_or(Value::Null),
        "limits": work.get("limits").cloned().unwrap_or(Value::Null),
        "reopen": work.get("reopen").cloned().unwrap_or(Value::Null),
        "deferred": front.get("deferred").unwrap_or(Value::Null),
        "created_at": front.get("created_at")?,
        "condition": condition,
    });
    serde_json::from_value(value).ok()
}

impl Vault {
    /// Directory this device alone writes.
    pub fn device_work_dir(&self) -> Result<String> {
        Ok(format!("{}/{}", self.config.work_dir(), self.device_id()?))
    }

    pub(crate) fn owns_work_path(&self, path: &str) -> bool {
        self.device_work_dir()
            .is_ok_and(|dir| path.starts_with(&format!("{dir}/")))
    }

    /// Move flat engine work notes through the existing crash-safe move path.
    pub(crate) fn migrate_device_work(&self) -> Result<()> {
        let dir = self.config.work_dir();
        let Some((parent, _)) =
            crate::contained::Dir::open_parent(self.root(), &format!("{dir}/item"))?
        else {
            return Ok(());
        };
        for name in parent.names()? {
            if !name.ends_with(".md") {
                continue;
            }
            let from = format!("{dir}/{name}");
            let Some(bytes) = read_rel(self.root(), &from)? else {
                continue;
            };
            let Ok(note) = Note::parse(&bytes) else {
                continue;
            };
            let Some(item) = work_item(&note) else {
                continue;
            };
            if item.path(dir) != from
                || wi_lifecycle::work::work_id(item.kind, &item.target, &item.related) != item.id
            {
                continue;
            }
            let to = format!("{}/{name}", self.device_work_dir()?);
            // An interrupted move is replayed before this method runs. A
            // pre-existing destination with other bytes is never replaced.
            if read_rel(self.root(), &to)?.is_some() {
                continue;
            }
            let target = crate::paths::Target {
                abs: self.root().join(&to),
                rel: to.clone(),
            };
            let source = crate::paths::Target {
                abs: self.root().join(&from),
                rel: from,
            };
            self.mark_index_pending(&[&source.rel, &target.rel])?;
            self.publish(crate::publish::Change {
                target,
                bytes: bytes.clone(),
                expect: crate::publish::Expect::Absent,
                moved_from: Some((source, wi_history::hash_bytes(&bytes))),
                op_id: None,
                digest: wi_history::hash_bytes(to.as_bytes()),
                ingress: crate::Ingress::Engine,
                revert_of: None,
                charge: None,
            })?;
        }
        Ok(())
    }

    /// Every work note, sorted by path.
    pub fn work_list(&self) -> Result<Vec<WorkEntry>> {
        let notes = self.lifecycle_notes_where(|n| {
            n.lifecycle.lifecycle_type == wi_note::LifecycleType::Work
        })?;
        Ok(notes
            .iter()
            .map(|n| {
                let work = n.lifecycle.work.as_ref();
                let state = work.and_then(|w| w.state.as_ref()).and_then(|s| {
                    serde_json::to_value(s)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                });
                WorkEntry {
                    path: n.path.clone(),
                    work_id: n.work_id.clone(),
                    work_kind: None,
                    target: work.and_then(|w| w.target.clone()),
                    state,
                    outcome: work.and_then(|w| w.outcome.clone()),
                    reason: work.and_then(|w| w.reason.clone()),
                    next_action: work.and_then(|w| w.next_action.clone()),
                    deferred: n.deferred.is_some(),
                    actionable: self.owns_work_path(&n.path),
                }
            })
            .map(|entry| {
                let kind = read_rel(self.root(), &entry.path)
                    .ok()
                    .flatten()
                    .and_then(|b| Note::parse(&b).ok())
                    .and_then(|n| text(&n, "work_kind"));
                WorkEntry {
                    work_kind: kind,
                    ..entry
                }
            })
            .collect())
    }

    /// Close a work note with an outcome. Outcomes other than `adopted`,
    /// `narrowed` and `alternative` need limits and a reopen trigger.
    pub fn work_close(&self, request: &WorkClose) -> Result<String> {
        let rel = crate::paths::normalize(self.root(), &request.path)?;
        if crate::paths::is_under(&rel, self.config.work_dir()) && !self.owns_work_path(&rel) {
            return Err(StoreError::LifecycleRefused {
                code: "foreign-device-work",
                message: "This work item belongs to another device and is read-only here.".into(),
            });
        }
        let bytes = read_rel(self.root(), &rel)?
            .ok_or_else(|| StoreError::NotFound { path: rel.clone() })?;
        let note = Note::parse(&bytes)?;
        let item =
            self.engine_work_item(&rel, &note)
                .ok_or_else(|| StoreError::LifecycleRefused {
                    code: "not-work-note",
                    message: format!("{rel} is not an engine-created work note"),
                })?;
        let reopen = request.reopen_trigger.map(|trigger| Reopen {
            trigger,
            detail: request.reopen_detail.clone().unwrap_or_default(),
        });
        let closed = item
            .close(request.outcome, request.limits.clone(), reopen)
            .map_err(|e| StoreError::LifecycleRefused {
                code: "work-close-refused",
                message: e.to_string(),
            })?;
        self.refresh_work(&rel, &closed)?;
        Ok(rel)
    }

    /// The work item of `note` at `rel` when it is an engine-created work
    /// note: `type: work`, under `work_dir`, at the path its kind, target and
    /// related notes determine, with the id they determine.
    pub(crate) fn engine_work_item(&self, rel: &str, note: &Note) -> Option<WorkItem> {
        let item = work_item(note)?;
        let id = wi_lifecycle::work::work_id(item.kind, &item.target, &item.related);
        (id == item.id && item.path(&self.device_work_dir().ok()?) == rel).then_some(item)
    }

    /// Refresh the work note at `path` with a freshly computed item while
    /// keeping its history: an open note keeps its creation time and
    /// progress state; a closed one is reopened only when its reopen
    /// trigger matches and the observed condition differs from the one
    /// recorded at closure. Returns whether bytes changed.
    pub(crate) fn refresh_sweep_item(&self, path: &str, item: &WorkItem) -> Result<bool> {
        let existing = read_rel(self.root(), path)?
            .and_then(|b| Note::parse(&b).ok())
            .and_then(|n| work_item(&n));
        let Some(old) = existing else {
            return self.refresh_work(path, item);
        };
        let next = if old.state == State::Closed {
            let observed = old.condition.as_deref();
            let reopens = old
                .reopen
                .as_ref()
                .is_some_and(|r| r.trigger == item.next_trigger)
                && observed.is_some_and(|o| item.condition.as_deref() != Some(o));
            if !reopens {
                return Ok(false);
            }
            WorkItem {
                created_at: old.created_at,
                ..item.clone()
            }
        } else {
            WorkItem {
                created_at: old.created_at,
                state: old.state,
                deferred: old.deferred.clone(),
                ..item.clone()
            }
        };
        self.refresh_work(path, &next)
    }

    /// Open a conflict work item for two claims the slot rule cannot see.
    pub fn work_open_conflict(&self, a: &str, b: &str, reason: &str) -> Result<String> {
        for path in [a, b] {
            if read_rel(self.root(), path)?.is_none() {
                return Err(StoreError::NotFound {
                    path: path.to_owned(),
                });
            }
        }
        let item = self
            .lifecycle_engine()
            .open_conflict_work(a, b, reason, self.lifecycle_now());
        self.write_work(&item)
    }

    /// Close the work note carrying deferred entry `id`: as adopted, or as
    /// not supported with the re-authorization `refused` reason.
    pub(crate) fn close_deferred_work(
        &self,
        snapshot: &Snapshot,
        id: &str,
        refused: Option<&str>,
    ) -> Result<()> {
        let holder = snapshot.notes().iter().find(|n| {
            if !self.owns_work_path(&n.path) {
                return false;
            }
            n.deferred
                .as_ref()
                .and_then(|d| d.get("id"))
                .and_then(Value::as_str)
                == Some(id)
        });
        let Some(holder) = holder else {
            return Ok(());
        };
        let Some(bytes) = read_rel(self.root(), &holder.path)? else {
            return Ok(());
        };
        let note = Note::parse(&bytes)?;
        if let Some(item) = work_item(&note)
            && item.state != State::Closed
            && let Ok(closed) = match refused {
                None => item.close(Outcome::Adopted, None, None),
                Some(reason) => item.close(
                    Outcome::NotSupported,
                    Some(format!("re-authorization at execution refused: {reason}")),
                    Some(Reopen {
                        trigger: Trigger::Session,
                        detail: "queue the change again if it is still wanted".into(),
                    }),
                ),
            }
        {
            let closed = WorkItem {
                deferred: None,
                ..closed
            };
            self.refresh_work(&holder.path, &closed)?;
        }
        Ok(())
    }

    /// The work item a per-device request becomes; `None` for gate
    /// requests (adoption runs on every write).
    fn request_item(&self, request: &WorkRequest) -> Option<WorkItem> {
        let kind = match request.kind {
            RequestKind::Gate => return None,
            RequestKind::ConflictCopy
            | RequestKind::RecoveryConflict
            | RequestKind::MergeConflict => WorkKind::Conflict,
            RequestKind::Missing
            | RequestKind::Quarantine
            | RequestKind::RecoveryProblem
            | RequestKind::IngestFailed => WorkKind::Recheck,
        };
        let code = serde_json::to_value(request.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let at = chrono::DateTime::from_timestamp_millis(
            i64::try_from(request.time_ms).unwrap_or(i64::MAX),
        )
        .unwrap_or_else(|| self.lifecycle_now());
        Some(WorkItem::new(
            kind,
            &request.path,
            vec![format!("{code}: {}", request.detail)],
            format!("{code}: {}", request.detail),
            "inspect the file and its git history, then close with an outcome",
            Trigger::Session,
            at,
        ))
    }

    /// Turn per-device work requests (`.git/wi/work-requests.jsonl`) into
    /// synced work notes. Requests already converted map to the same id.
    pub fn convert_work_requests(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for request in self.work_requests()? {
            let Some(item) = self.request_item(&request) else {
                continue;
            };
            let path = item.path(&self.device_work_dir()?);
            if read_rel(self.root(), &path)?.is_none() {
                out.push(self.write_work(&item)?);
            }
        }
        Ok(out)
    }

    /// The cause of `request` was seen again: reopen its work note when a
    /// maintenance pass had closed it as resolved. A note not converted
    /// yet is left to [`Self::convert_work_requests`].
    pub(crate) fn reopen_resolved_request(&self, request: &WorkRequest) -> Result<()> {
        if !self.lifecycle_on() {
            return Ok(());
        }
        let Some(item) = self.request_item(request) else {
            return Ok(());
        };
        let path = item.path(&self.device_work_dir()?);
        if read_rel(self.root(), &path)?.is_some() {
            // `write_work` reopens a resolved note and keeps any other one.
            self.write_work(&item)?;
        }
        Ok(())
    }
}
