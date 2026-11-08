//! Applying engine outputs through the write path as the `engine` actor:
//! byte-preserving frontmatter edits, engine-built notes and work notes.

use serde_json::json;
use wi_history::hash_bytes;
use wi_lifecycle::{NoteEdit, WorkItem};
use wi_note::Note;

use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::lifecycle::Charge;
use crate::paths::{Target, check_protected_managed, normalize};
use crate::publish::{Change, Expect, request_digest};
use crate::vault::{Ingress, Vault, WriteResult};

impl Vault {
    fn engine_target(&self, path: &str) -> Result<Target> {
        let rel = normalize(self.root(), path)?;
        check_protected_managed(&self.config, &rel)?;
        Ok(Target {
            abs: self.root().join(&rel),
            rel,
        })
    }

    /// Publish engine-built bytes. `expect` is the hash the decision saw
    /// (`None`: the path must be absent). `actor` names the ingress that
    /// caused the change (the engine for bookkeeping, MCP for an agent's
    /// claim revision).
    pub(crate) fn publish_engine_bytes(
        &self,
        path: &str,
        bytes: Vec<u8>,
        expect: Option<String>,
        actor: Ingress,
        charge: Option<Charge>,
    ) -> Result<WriteResult> {
        let target = self.engine_target(path)?;
        let digest = request_digest(&json!({
            "tool": "lifecycle",
            "path": target.rel,
            "content": hash_bytes(&bytes),
            "expected": expect,
        }));
        self.publish(Change {
            target,
            bytes,
            expect: match expect {
                Some(hash) => Expect::Hash(hash),
                None => Expect::Absent,
            },
            moved_from: None,
            op_id: None,
            digest,
            ingress: actor,
            revert_of: None,
            charge,
        })
    }

    /// Apply one engine edit to the current bytes of its note. Returns
    /// `None` when the edit changes nothing or the note is gone.
    pub(crate) fn apply_edit(
        &self,
        edit: &NoteEdit,
        expect: Option<&str>,
        charge: Option<Charge>,
    ) -> Result<Option<WriteResult>> {
        let Some(bytes) = read_rel(self.root(), &edit.path)? else {
            return Ok(None);
        };
        let hash = hash_bytes(&bytes);
        if let Some(expected) = expect
            && expected != hash
        {
            return Err(StoreError::Stale {
                path: edit.path.clone(),
                expected: expected.to_owned(),
                current: Some(hash),
            });
        }
        let note = Note::parse(&bytes)?;
        let edited = edit
            .apply(&note)
            .map_err(|e| StoreError::Edit(format!("lifecycle edit of {}: {e}", edit.path)))?;
        if edited.to_bytes() == bytes.as_slice() && charge.is_none() {
            return Ok(None);
        }
        self.publish_engine_bytes(
            &edit.path,
            edited.to_bytes().to_vec(),
            Some(hash),
            Ingress::Engine,
            charge,
        )
        .map(Some)
    }

    /// Write a work note unless one with the same id exists. An item that
    /// maintenance closed because its cause was gone is opened again with
    /// the new reason. Returns its path.
    pub(crate) fn write_work(&self, item: &WorkItem) -> Result<String> {
        let path = item.path(&self.device_work_dir()?);
        let existing = read_rel(self.root(), &path)?;
        let resolved = existing
            .as_deref()
            .and_then(|b| wi_note::Note::parse(b).ok())
            .and_then(|n| self.engine_work_item(&path, &n))
            .is_some_and(|old| {
                old.state == wi_lifecycle::State::Closed
                    && old.outcome == Some(wi_lifecycle::Outcome::Resolved)
            });
        if resolved {
            self.refresh_work(&path, item)?;
        } else if existing.is_none() {
            self.publish_engine_bytes(
                &path,
                item.render().into_bytes(),
                None,
                Ingress::Engine,
                None,
            )?;
        }
        Ok(path)
    }

    /// Replace an existing work note with a refreshed rendering.
    pub(crate) fn refresh_work(&self, path: &str, item: &WorkItem) -> Result<bool> {
        if !self.owns_work_path(path) {
            return Err(StoreError::LifecycleRefused {
                code: "foreign-device-work",
                message: "This work item belongs to another device.".into(),
            });
        }
        let Some(bytes) = read_rel(self.root(), path)? else {
            self.write_work(item)?;
            return Ok(true);
        };
        let fresh = item.render().into_bytes();
        if fresh == bytes {
            return Ok(false);
        }
        self.publish_engine_bytes(path, fresh, Some(hash_bytes(&bytes)), Ingress::Engine, None)?;
        Ok(true)
    }
}
