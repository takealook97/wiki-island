//! Writing conversation notes and the import record that makes re-imports
//! idempotent.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::json;
use wi_history::hash_bytes;
use wi_note::remove_key;
use wi_store::{IMPORTS_DIR, Ingress, Vault, WriteRequest};

use super::render::{Draft, parts};
use super::{ConversationCounts, Kind};
use crate::notion::checked_path;
use crate::{Error, Result, field, optional, parse, render};

const CHECKSUM: &str = "conversation_checksum";
/// Characters of the conversation id added to a name that collides.
const ID_SUFFIX: usize = 8;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    paths: Vec<String>,
    checksums: Vec<String>,
}

pub(crate) struct Applier<'a> {
    vault: &'a Vault,
    kind: Kind,
    bound: usize,
    manifest_path: String,
    old_manifest: Option<wi_store::ReadResult>,
    entries: BTreeMap<String, Entry>,
    used: BTreeSet<String>,
    pub counts: ConversationCounts,
}

/// The hash of a note as it was written, without store-owned and checksum keys.
fn editable_hash(bytes: &[u8]) -> Result<String> {
    let mut note = parse(bytes)?;
    for key in ["origin", "imported_at", CHECKSUM] {
        note = remove_key(&note, key).map_err(|e| Error::Invalid(e.to_string()))?;
    }
    Ok(hash_bytes(note.to_bytes()))
}

fn id_suffix(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(ID_SUFFIX)
        .collect()
}

fn part_path(folder: &str, stem: &str, index: usize) -> String {
    if index == 0 {
        format!("{folder}/{stem}.md")
    } else {
        format!("{folder}/{stem} ({}).md", index + 1)
    }
}

impl<'a> Applier<'a> {
    pub(crate) fn new(vault: &'a Vault, kind: Kind) -> Result<Self> {
        let manifest_path = format!("{IMPORTS_DIR}/conversations/{}.md", kind.slug());
        let old_manifest = optional(vault, &manifest_path)?;
        let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
        if let Some(old) = &old_manifest {
            let note = parse(&old.bytes)?;
            let value = field(&note, "properties")
                .and_then(|p| p.get("conversations").cloned())
                .ok_or_else(|| Error::Conflict(manifest_path.clone()))?;
            entries = serde_json::from_value(value)
                .map_err(|_| Error::Conflict(manifest_path.clone()))?;
        }
        let mut used = BTreeSet::new();
        for entry in entries.values() {
            for path in &entry.paths {
                checked_path(path)?;
                if !path.starts_with("AI/") || !path.ends_with(".md") {
                    return Err(Error::Conflict(manifest_path));
                }
                used.insert(path.to_lowercase());
            }
        }
        let bound = crate::current_note_bound(vault)?;
        Ok(Self {
            vault,
            kind,
            bound,
            manifest_path,
            old_manifest,
            entries,
            used,
            counts: ConversationCounts::default(),
        })
    }

    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }

    /// A first path no other note uses; a note of the same conversation
    /// already at a path (an interrupted run) is adopted.
    fn allocate(
        &mut self,
        id: &str,
        folder: &str,
        stem: &str,
        count: usize,
    ) -> Result<Vec<String>> {
        let suffix = id_suffix(id);
        let mut candidates = vec![stem.to_owned()];
        if !suffix.is_empty() {
            candidates.push(format!("{stem} {suffix}"));
        }
        for number in 2..1000 {
            candidates.push(format!("{stem} {suffix} {number}"));
        }
        for candidate in candidates {
            let paths: Vec<String> = (0..count)
                .map(|i| part_path(folder, &candidate, i))
                .collect();
            let mut free = true;
            for path in &paths {
                checked_path(path)?;
                if self.used.contains(&path.to_lowercase()) {
                    free = false;
                    break;
                }
                if let Some(existing) = optional(self.vault, path)? {
                    let note = parse(&existing.bytes)?;
                    if field(&note, "conversation_id") != Some(json!(id)) {
                        free = false;
                        break;
                    }
                }
            }
            if free {
                return Ok(paths);
            }
        }
        Err(Error::Invalid("too many notes share one name".into()))
    }

    /// Write the notes of one conversation or document.
    pub(crate) fn apply(
        &mut self,
        id: &str,
        folder: &str,
        stem: &str,
        draft: &Draft,
    ) -> Result<()> {
        let bases = parts(draft, self.bound)?;
        let old = self.entries.get(id).cloned();
        let paths = match &old {
            Some(entry) if !entry.paths.is_empty() => self.extend_paths(entry, bases.len())?,
            _ => self.allocate(id, folder, stem, bases.len())?,
        };
        let mut checksums = Vec::new();
        for (index, (path, base)) in paths.iter().zip(&bases).enumerate() {
            let rendered = hash_bytes(base.as_bytes());
            let kept = self.write_part(id, path, base, &rendered)?;
            checksums.push(kept.unwrap_or_else(|| {
                old.as_ref()
                    .and_then(|e| e.checksums.get(index).cloned())
                    .unwrap_or(rendered)
            }));
        }
        for path in &paths {
            self.used.insert(path.to_lowercase());
        }
        self.entries
            .insert(id.to_owned(), Entry { paths, checksums });
        Ok(())
    }

    fn extend_paths(&mut self, entry: &Entry, count: usize) -> Result<Vec<String>> {
        let mut paths: Vec<String> = entry.paths.iter().take(count).cloned().collect();
        let first = &entry.paths[0];
        let (folder, name) = first.rsplit_once('/').unwrap_or(("", first));
        let stem = name.strip_suffix(".md").unwrap_or(name);
        for index in paths.len()..count {
            let path = part_path(folder, stem, index);
            checked_path(&path)?;
            paths.push(path);
        }
        Ok(paths)
    }

    /// Returns the checksum to record when it differs from the rendered one.
    fn write_part(
        &mut self,
        id: &str,
        path: &str,
        base: &str,
        rendered: &str,
    ) -> Result<Option<String>> {
        let existing = optional(self.vault, path)?;
        if let Some(old) = &existing {
            let note = parse(&old.bytes)?;
            let stored = field(&note, CHECKSUM);
            let ours = field(&note, "conversation_id") == Some(json!(id));
            if !ours || stored != Some(json!(editable_hash(&old.bytes)?)) {
                self.counts.conflicts += 1;
                self.counts.conflict_paths.push(path.to_owned());
                return Ok(stored.and_then(|v| v.as_str().map(str::to_owned)));
            }
            if stored == Some(json!(rendered)) {
                self.counts.unchanged += 1;
                return Ok(None);
            }
        }
        let note = wi_note::set_key(&parse(base.as_bytes())?, CHECKSUM, &json!(rendered))
            .map_err(|e| Error::Invalid(e.to_string()))?;
        self.check_size(&note, existing.as_ref(), path)?;
        self.vault.write(WriteRequest {
            path: path.to_owned(),
            content: note.to_bytes().to_vec(),
            expected_checksum: existing.as_ref().map(|v| v.checksum.clone()),
            op_id: None,
            ingress: Ingress::Import,
        })?;
        if existing.is_some() {
            self.counts.updated += 1;
        } else {
            self.counts.created += 1;
        }
        Ok(None)
    }

    fn check_size(
        &self,
        note: &wi_note::Note,
        old: Option<&wi_store::ReadResult>,
        path: &str,
    ) -> Result<()> {
        let origin = match old {
            Some(old) => field(&parse(&old.bytes)?, "origin"),
            None => Some(
                json!({"kind":"file", "locator":"import:write", "captured_at":"2000-01-01T00:00:00Z"}),
            ),
        };
        let size = match origin {
            Some(value) => wi_note::set_key(note, "origin", &value)
                .map_err(|e| Error::Invalid(e.to_string()))?
                .to_bytes()
                .len(),
            None => note.to_bytes().len(),
        };
        if size > self.bound {
            return Err(Error::Invalid(format!(
                "{path} exceeds configured note bound"
            )));
        }
        Ok(())
    }

    /// Record the notes; nothing is written when the record is unchanged.
    pub(crate) fn finish(self) -> Result<(String, ConversationCounts)> {
        let properties = json!({"conversations": self.entries});
        let fields = json!({
            "type": "note",
            "conversation_manifest": self.kind.slug(),
            "properties": properties,
        });
        let content = render(fields, "# Conversation import\n")?;
        let current = match &self.old_manifest {
            Some(old) => {
                let mut note = parse(&old.bytes)?;
                for key in ["origin", "imported_at"] {
                    note = remove_key(&note, key).map_err(|e| Error::Invalid(e.to_string()))?;
                }
                Some(note.to_bytes().to_vec())
            }
            None => None,
        };
        if current.as_deref() != Some(content.as_bytes()) {
            self.vault.write_import_record(WriteRequest {
                path: self.manifest_path.clone(),
                content: content.into_bytes(),
                expected_checksum: self.old_manifest.map(|m| m.checksum),
                op_id: None,
                ingress: Ingress::Import,
            })?;
        }
        Ok((self.manifest_path, self.counts))
    }
}
