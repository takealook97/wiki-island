//! `wi capture` / `wi_capture`: a capture note, an optional URL snapshot
//! with its observation event, and optional proposed candidates. Capture
//! never adopts by itself; proposals pass the filter and then the gates.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Value, json};
use wi_history::hash_bytes;
use wi_lifecycle::FilterOutcome;

use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::lifecycle::evidence::{Observation, Observed, sidecar_path};
use crate::ops::WriteRequest;
use crate::vault::{Ingress, Vault};

/// A caller-supplied URL snapshot to store; the engine does not fetch it.
#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotInput {
    pub url: String,
    pub path: String,
    pub content: String,
}

/// A proposed memory candidate.
#[derive(Debug, Clone, Deserialize)]
pub struct Proposal {
    pub path: String,
    pub content: String,
}

/// Capture arguments (PROTOCOL `wi_capture`).
#[derive(Debug, Clone, Deserialize)]
pub struct CaptureInput {
    pub text: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub no_memory_reason: Option<String>,
    #[serde(default)]
    pub snapshot: Option<SnapshotInput>,
    #[serde(default)]
    pub proposals: Vec<Proposal>,
}

fn reasons(outcome: &FilterOutcome) -> Value {
    match outcome {
        FilterOutcome::Drop(reasons) => json!(reasons),
        FilterOutcome::Empty => json!(["empty"]),
        FilterOutcome::Accept => json!([]),
    }
}

impl Vault {
    fn body_hashes(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .lifecycle_notes()?
            .into_iter()
            .filter_map(|n| n.body_hash)
            .collect())
    }

    /// Store a supplied URL snapshot and record its observation and ingress.
    /// The same bytes add only the event; new bytes replace the snapshot.
    pub fn record_snapshot(&self, input: &SnapshotInput, ingress: Ingress) -> Result<Value> {
        let rel = crate::paths::normalize(self.root(), &input.path)?;
        let captures = self.config.captures_dir().trim_matches('/');
        let legacy = self.config.legacy_captures_dir();
        let under = |dir: &str| crate::paths::is_under(&rel, dir) && rel.len() > dir.len() + 1;
        if !(under(captures) || under(&legacy)) || rel.ends_with(".md") {
            return Err(StoreError::Invalid(format!(
                "a URL snapshot is a non-Markdown file under {captures}/"
            )));
        }
        let bytes = input.content.as_bytes().to_vec();
        let hash = hash_bytes(&bytes);
        let current = read_rel(self.root(), &rel)?.map(|b| hash_bytes(&b));
        if current.as_deref() != Some(hash.as_str()) {
            self.publish_engine_bytes(&rel, bytes, current, ingress, None)?;
        }
        let sidecar = sidecar_path(&rel);
        let old = read_rel(self.root(), &sidecar)?;
        let observed = old
            .as_deref()
            .and_then(|b| serde_json::from_slice::<Observed>(b).ok())
            .unwrap_or_default();
        let at = self.now_text();
        let observed_by = ingress.origin_kind().unwrap_or("unknown");
        let next = Observed {
            url: input.url.clone(),
            observations: observed
                .observations
                .into_iter()
                .chain([Observation {
                    at: at.clone(),
                    hash: hash.clone(),
                    observed_by: observed_by.into(),
                }])
                .collect(),
        };
        let text = serde_json::to_vec_pretty(&next)
            .map_err(|e| StoreError::Invalid(format!("observation: {e}")))?;
        self.publish_engine_bytes(
            &sidecar,
            text,
            old.as_deref().map(hash_bytes),
            Ingress::Engine,
            None,
        )?;
        Ok(
            json!({"path": rel, "source_hash": hash, "observed_at": at, "observed_by": observed_by, "url": input.url}),
        )
    }

    /// Capture text (and optionally a snapshot and proposals).
    pub fn capture(&self, input: &CaptureInput, ingress: Ingress) -> Result<Value> {
        let engine = self.lifecycle_engine();
        let existing = self.body_hashes()?;
        let filtered = engine.capture_filter(&input.text, &existing);
        let empty_ok = input
            .no_memory_reason
            .as_deref()
            .is_some_and(|r| !r.trim().is_empty());
        match &filtered {
            FilterOutcome::Drop(_) => {
                return Err(StoreError::LifecycleRefused {
                    code: "capture-dropped",
                    message: format!("capture filter: {}", reasons(&filtered)),
                });
            }
            FilterOutcome::Empty if !empty_ok => {
                return Err(StoreError::LifecycleRefused {
                    code: "capture-empty",
                    message: "an empty capture needs a no_memory_reason".into(),
                });
            }
            _ => {}
        }
        let snapshot = match &input.snapshot {
            Some(s) => Some(self.record_snapshot(s, ingress)?),
            None => None,
        };
        let mut proposals = Vec::new();
        let mut outputs = Vec::new();
        for proposal in &input.proposals {
            let outcome = engine.capture_filter(&proposal.content, &existing);
            if !matches!(outcome, FilterOutcome::Accept) {
                proposals.push(json!({"path": proposal.path, "dropped": reasons(&outcome)}));
                continue;
            }
            let result = self.lifecycle_write(WriteRequest {
                path: proposal.path.clone(),
                content: proposal.content.as_bytes().to_vec(),
                expected_checksum: None,
                op_id: None,
                ingress,
            })?;
            outputs.push(result.path.clone());
            proposals.push(json!({"path": result.path, "lifecycle": result.lifecycle}));
        }
        let now = self.lifecycle_now();
        let stamp = now.format("%Y%m%d-%H%M%S");
        let short = &hash_bytes(input.text.as_bytes())[7..15];
        let path = format!(
            "{}/{stamp}-{short}.md",
            self.config.captures_dir().trim_matches('/')
        );
        let resolved = !outputs.is_empty() || empty_ok;
        let title = input
            .title
            .clone()
            .unwrap_or_else(|| format!("capture {stamp}"));
        let mut front = vec![
            format!("title: {}", json!(title)),
            "type: capture".into(),
            format!(
                "capture_status: {}",
                if resolved { "resolved" } else { "pending" }
            ),
            format!("outputs: {}", json!(outputs)),
        ];
        if let Some(reason) = input.no_memory_reason.as_deref().filter(|_| empty_ok) {
            front.push(format!("no_memory_reason: {}", json!(reason)));
        }
        let content = format!("---\n{}\n---\n\n{}\n", front.join("\n"), input.text.trim());
        // The capture path is the engine's choice under a managed folder.
        self.managed_write.set(true);
        let written = self.write(WriteRequest {
            path: path.clone(),
            content: content.into_bytes(),
            expected_checksum: None,
            op_id: None,
            ingress,
        });
        self.managed_write.set(false);
        let written = written?;
        Ok(json!({
            "capture": written.path,
            "capture_status": if resolved { "resolved" } else { "pending" },
            "snapshot": snapshot,
            "proposals": proposals,
        }))
    }
}
