//! The durable deferred queue for capped changes.
//!
//! Representation: each entry lives in the `deferred` frontmatter key of its
//! own open `type: work` note, so the queue is in the vault, syncs with it and
//! is rebuilt from the notes after index loss. Order: replacements backed by
//! newer source evidence first, then FIFO by `enqueued_at`, then id.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use wi_note::WorkState;

use crate::budget::{ActiveCounts, BudgetLedger, LossEvent, LossRequest};
use crate::model::{Actor, Snapshot};
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "action")]
pub enum DeferredAction {
    /// Adopt a candidate and supersede its valid targets.
    Adopt { candidate: String },
    /// Archive, move into the archive, exclude or flag a conflict.
    Loss,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredEntry {
    pub id: String,
    pub action: DeferredAction,
    pub requests: Vec<LossRequest>,
    pub actor: Actor,
    pub enqueued_at: Timestamp,
    /// A replacement whose candidate evidence is newer than its targets'.
    pub newer_evidence: bool,
    /// `sha256:` of the target note when the change was queued. Resumption
    /// re-authorizes against the live note and refuses a changed target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_hash: Option<String>,
}

impl DeferredEntry {
    fn order_key(&self) -> (bool, Timestamp, &str) {
        (!self.newer_evidence, self.enqueued_at, self.id.as_str())
    }

    fn entities(&self) -> BTreeSet<&str> {
        self.requests
            .iter()
            .flat_map(|r| r.entities.iter().map(String::as_str))
            .collect()
    }
}

fn merge(earlier: &DeferredEntry, later: DeferredEntry) -> DeferredEntry {
    DeferredEntry {
        enqueued_at: earlier.enqueued_at.min(later.enqueued_at),
        newer_evidence: earlier.newer_evidence || later.newer_evidence,
        ..later
    }
}

/// Entries that may apply now, plus the state after applying them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub ready: Vec<(DeferredEntry, Vec<LossEvent>)>,
    pub remaining: DeferredQueue,
    pub ledger: BudgetLedger,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredQueue {
    entries: Vec<DeferredEntry>,
}

impl DeferredQueue {
    /// One entry per id. A repeated id merges: the later entry's action and
    /// requests, the earliest `enqueued_at` (FIFO position is kept) and the
    /// higher evidence priority.
    pub fn new(entries: Vec<DeferredEntry>) -> Self {
        let merged =
            entries
                .into_iter()
                .fold(BTreeMap::<String, DeferredEntry>::new(), |acc, entry| {
                    let next = match acc.get(&entry.id) {
                        Some(earlier) => merge(earlier, entry),
                        None => entry,
                    };
                    let mut acc = acc;
                    acc.insert(next.id.clone(), next);
                    acc
                });
        let mut entries: Vec<DeferredEntry> = merged.into_values().collect();
        entries.sort_by(|a, b| a.order_key().cmp(&b.order_key()));
        Self { entries }
    }

    /// Rebuild from open work notes carrying a `deferred` entry.
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        let entries = snapshot
            .notes()
            .iter()
            .filter(|n| {
                n.lifecycle
                    .work
                    .as_ref()
                    .is_none_or(|w| w.state != Some(WorkState::Closed))
            })
            .filter_map(|n| serde_json::from_value(n.deferred.clone()?).ok())
            .collect();
        Self::new(entries)
    }

    pub fn entries(&self) -> &[DeferredEntry] {
        &self.entries
    }

    pub fn push(&self, entry: DeferredEntry) -> Self {
        Self::new(self.entries.iter().cloned().chain([entry]).collect())
    }

    /// Apply entries in order while the budget allows. An entry that does not
    /// fit blocks later entries on the same entities, so per-entity order holds.
    pub fn resume(&self, ledger: &BudgetLedger, counts: &ActiveCounts, now: Timestamp) -> Resume {
        let mut ledger = ledger.clone();
        let mut counts = counts.clone();
        let mut blocked: BTreeSet<String> = BTreeSet::new();
        let mut ready = Vec::new();
        let mut remaining = Vec::new();
        for entry in &self.entries {
            let entities = entry.entities();
            if entities.iter().any(|e| blocked.contains(*e)) {
                remaining.push(entry.clone());
                continue;
            }
            match ledger.reserve(&entry.requests, entry.actor, &counts, now) {
                Ok((next, events)) => {
                    ledger = next;
                    counts = counts.after(&entry.requests);
                    ready.push((entry.clone(), events));
                }
                Err(_) => {
                    blocked.extend(entities.iter().map(|e| e.to_string()));
                    remaining.push(entry.clone());
                }
            }
        }
        Resume {
            ready,
            remaining: Self::new(remaining),
            ledger,
        }
    }
}
