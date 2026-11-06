//! Work-item requests record what needs attention and become `type: work` notes.
//! Requests are appended to
//! `.git/wi/work-requests.jsonl` (per device, like the manifest) with a
//! file barrier, and a repeated request (same kind, path and detail) is
//! recorded once.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use wi_history::Vault as HistoryVault;
use wi_history::durable;

use crate::error::{Result, StoreError};

const FILE: &str = "work-requests.jsonl";

/// What a work request is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkKind {
    /// An editor save displaced by an engine write, kept as a conflict copy.
    ConflictCopy,
    /// Crash recovery found a third version (neither old nor new bytes).
    RecoveryConflict,
    /// A file with git merge conflict markers.
    MergeConflict,
    /// A tracked note deleted outside the engine.
    Missing,
    /// A corrupt manifest tail moved to quarantine.
    Quarantine,
    /// A lifecycle note changed by file ingress; lifecycle gates must run.
    Gate,
    /// Crash recovery found a persisted path whose directory changed (a
    /// symlink or a non-directory) and left it untouched.
    RecoveryProblem,
    /// A changed file that file ingress could not read or publish; it is
    /// not in history until the cause is fixed.
    IngestFailed,
}

/// One recorded request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRequest {
    pub kind: WorkKind,
    pub path: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub time_ms: u64,
}

impl WorkRequest {
    pub(crate) fn new(kind: WorkKind, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.into(),
            detail: detail.into(),
            seq: None,
            time_ms: now_ms(),
        }
    }

    pub(crate) fn with_seq(self, seq: u64) -> Self {
        Self {
            seq: Some(seq),
            ..self
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn path(vault: &HistoryVault) -> PathBuf {
    vault.wi_dir().join(FILE)
}

/// Every recorded request, oldest first. Unparseable lines are skipped.
pub fn list(vault: &HistoryVault) -> Result<Vec<WorkRequest>> {
    let file = path(vault);
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(StoreError::io(file, e)),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// Record `request` unless an equal one (kind, path, detail) exists.
pub(crate) fn record(vault: &HistoryVault, request: &WorkRequest) -> Result<bool> {
    let existing = list(vault)?;
    if existing
        .iter()
        .any(|r| r.kind == request.kind && r.path == request.path && r.detail == request.detail)
    {
        return Ok(false);
    }
    let file = path(vault);
    durable::create_dir_durable(&vault.wi_dir(), vault.durability())?;
    let created = !file.exists();
    let mut handle = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .map_err(|e| StoreError::io(&file, e))?;
    let mut line = serde_json::to_vec(request)
        .map_err(|e| StoreError::Invalid(format!("work request: {e}")))?;
    line.push(b'\n');
    handle
        .write_all(&line)
        .map_err(|e| StoreError::io(&file, e))?;
    durable::barrier(&handle, vault.durability()).map_err(|e| StoreError::io(&file, e))?;
    if created {
        durable::barrier_dir(&vault.wi_dir(), vault.durability())?;
    }
    Ok(true)
}
