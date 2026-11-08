//! Committed snapshots for read-only views. Reads the branch tip tree only,
//! never the working tree, and takes no engine lock.
use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::{Map, Value};
use wi_history::History;
use wi_note::{Note, Status};

use crate::error::{Result, StoreError};
use crate::paths::{is_engine_internal, is_markdown, is_under};
use crate::vault::Vault;

/// One Markdown note at the committed branch tip.
#[derive(Debug, Clone)]
pub struct CommittedNote {
    pub rel_path: String,
    pub bytes: Vec<u8>,
    /// Unique within the snapshot: the frontmatter `permalink` when free,
    /// else one generated from the path (with a numeric suffix on clashes).
    pub permalink: String,
    pub title: String,
    /// Archived by status or by living under the archive directory.
    pub archived: bool,
}

impl Vault {
    /// This vault's directory in the per-device data directory
    /// (`<data>/vaults/<vault_id>`), whatever the storage mode. Nothing in
    /// it is shared through the notes folder or the git remote.
    pub fn device_state_dir(&self) -> Result<PathBuf> {
        let data = self
            .history
            .data_root()
            .map(PathBuf::from)
            .or_else(wi_history::data_root)
            .ok_or_else(|| {
                StoreError::Invalid("No data directory is available on this device.".into())
            })?;
        Ok(wi_history::vault_state_dir(&data, self.vault_id())?)
    }

    /// Every owner-visible Markdown note at the branch tip, in path order.
    /// Engine folders, hidden files and conflict copies are left out.
    pub fn committed_notes(&self) -> Result<Vec<CommittedNote>> {
        let mut tree = History::tree(&self.history)?;
        tree.retain(|(rel, _)| {
            is_markdown(rel)
                && !is_engine_internal(rel)
                && !rel.split('/').any(|s| s.starts_with('.'))
        });
        tree.sort_by(|a, b| a.0.cmp(&b.0));
        let mut parsed = Vec::with_capacity(tree.len());
        for (rel, blob) in tree {
            let bytes = self.history.read_blob(&blob)?;
            let note = Note::parse(&bytes).ok();
            let metadata = note
                .as_ref()
                .and_then(|n| n.frontmatter())
                .map(|f| {
                    Value::Object(
                        f.entries
                            .iter()
                            .map(|e| (e.key.clone(), e.value.clone()))
                            .collect::<Map<_, _>>(),
                    )
                })
                .unwrap_or(Value::Null);
            let body = note.as_ref().map_or("", |n| n.body());
            let title = wi_note::hit_title(&rel, &metadata, body);
            let archived = is_under(&rel, self.config.archive_dir())
                || metadata.get("status").and_then(Value::as_str) == Some("archived")
                || note
                    .as_ref()
                    .is_some_and(|n| n.lifecycle().status == Some(Status::Archived));
            let explicit = metadata
                .get("permalink")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            parsed.push((rel, bytes, explicit, title, archived));
        }
        // Explicit permalinks claim first so a generated one never takes them.
        let mut taken = HashSet::new();
        let mut claimed = vec![false; parsed.len()];
        for (i, (_, _, explicit, _, _)) in parsed.iter().enumerate() {
            if let Some(p) = explicit
                && taken.insert(p.clone())
            {
                claimed[i] = true;
            }
        }
        let mut out = Vec::with_capacity(parsed.len());
        for (i, (rel, bytes, explicit, title, archived)) in parsed.into_iter().enumerate() {
            let permalink = match explicit {
                Some(p) if claimed[i] => p,
                _ => {
                    let p = wi_note::resolve_permalink(None, &rel, &taken)
                        .unwrap_or_else(|_| format!("note-{i}"));
                    taken.insert(p.clone());
                    p
                }
            };
            out.push(CommittedNote {
                rel_path: rel,
                bytes,
                permalink,
                title,
                archived,
            });
        }
        Ok(out)
    }
}
