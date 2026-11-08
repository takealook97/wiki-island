//! Owner adoption of an agent-written note (`wi adopt <note>`, CLI only).
//!
//! Provenance is carried by the note itself (AUTHORITY): the owner adopts
//! a note by removing its agent label in any editor, or with this command.
//! There is no MCP adopt tool, so an agent cannot launder its own note.

use serde_json::json;
use wi_note::{Note, remove_key, set_key};

use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::ops::is_locked;
use crate::paths::{Target, normalize};
use crate::publish::{Change, Expect, request_digest};
use crate::vault::{Ingress, Vault, WriteResult};
use crate::work::{WorkKind, WorkRequest};

impl Vault {
    /// Remove the agent label of the note at `path`: `origin.kind: agent`
    /// becomes `origin.kind: user` (locator `cli:adopt`) and a noted rule or
    /// preference loses `noted_by: agent`. `None` when the note carries no
    /// agent label. A lifecycle note gets a `gate` work request, like an
    /// adoption in an editor.
    pub fn take_ownership(&self, path: &str) -> Result<Option<WriteResult>> {
        let rel = normalize(self.root(), path)?;
        let Some(bytes) = read_rel(self.root(), &rel)? else {
            return Err(StoreError::NotFound { path: rel });
        };
        let note = Note::parse(&bytes).map_err(|e| StoreError::Edit(e.to_string()))?;
        let text = |key: &str| {
            note.frontmatter()
                .and_then(|f| f.get(key))
                .and_then(|v| v.as_str().map(str::to_owned))
        };
        let agent_origin = note
            .frontmatter()
            .and_then(|f| f.get("origin"))
            .and_then(|o| o.get("kind").and_then(|k| k.as_str().map(str::to_owned)))
            .as_deref()
            == Some("agent");
        let noted = text("noted_by").as_deref() == Some("agent");
        if !agent_origin && !noted {
            return Ok(None);
        }
        if is_locked(&note) {
            return Err(StoreError::Edit(format!("{rel} is locked")));
        }
        let mut edited = note.clone();
        if agent_origin {
            let origin = json!({
                "kind": "user",
                "locator": "cli:adopt",
                "captured_at": self.now_text(),
            });
            edited =
                set_key(&edited, "origin", &origin).map_err(|e| StoreError::Edit(e.to_string()))?;
        }
        if noted {
            edited =
                remove_key(&edited, "noted_by").map_err(|e| StoreError::Edit(e.to_string()))?;
        }
        let result = self.publish(Change {
            target: Target {
                rel: rel.clone(),
                abs: self.root().join(&rel),
            },
            bytes: edited.to_bytes().to_vec(),
            expect: Expect::Hash(wi_history::hash_bytes(&bytes)),
            moved_from: None,
            op_id: None,
            digest: request_digest(&json!({"tool": "adopt-ownership", "path": rel})),
            ingress: Ingress::Engine,
            revert_of: None,
            charge: None,
        })?;
        if agent_origin {
            self.record_work(
                WorkRequest::new(WorkKind::Gate, &rel, "owner adopted an agent note")
                    .with_seq(result.seq),
            )?;
        }
        Ok(Some(result))
    }
}
