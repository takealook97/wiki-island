use serde::{Deserialize, Serialize};
use wi_note::Note;

use super::MaintainReport;
use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::vault::Vault;

/// Per-device memory of the last pass (`.git/wi/maintain-state.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct DeviceState {
    /// Digest of the procedure notes the last pass saw.
    procedures: Option<String>,
}

const STATE_FILE: &str = "maintain-state.json";

impl Vault {
    /// Whether any procedure note changed since the last pass on this
    /// device; remembers the current set.
    pub(super) fn maintain_procedures(&self, report: &mut MaintainReport) -> Result<()> {
        let procedures =
            self.lifecycle_notes_where(|n| n.lifecycle.kind == Some(wi_note::Kind::Procedure))?;
        let digest = wi_lifecycle::sha256(
            serde_json::to_string(&procedures)
                .map_err(|e| StoreError::Invalid(e.to_string()))?
                .as_bytes(),
        );
        let file = self.history.wi_dir().join(STATE_FILE);
        let mut state: DeviceState = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if state.procedures.as_deref() == Some(digest.as_str()) {
            return Ok(());
        }
        report.procedures_changed = true;
        state.procedures = Some(digest);
        let text = serde_json::to_vec(&state).map_err(|e| StoreError::Invalid(e.to_string()))?;
        wi_history::durable::write_atomic(&file, &text, self.history.durability())?;
        Ok(())
    }
}

pub(super) fn note_has_type(vault: &Vault, rel: &str, kind: &str) -> bool {
    read_rel(vault.root(), rel)
        .ok()
        .flatten()
        .and_then(|bytes| Note::parse(&bytes).ok())
        .and_then(|note| {
            note.frontmatter()
                .and_then(|f| f.get("type"))
                .and_then(|v| v.as_str().map(str::to_owned))
        })
        .is_some_and(|t| t == kind)
}

/// Remove `dir` and its subdirectories when they hold nothing.
pub(super) fn remove_empty_dirs(dir: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                remove_empty_dirs(&entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}
