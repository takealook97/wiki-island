//! Work items (OPERATIONS "Work items"): `type: work` notes the engine proposes.

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

use crate::model::sha256;
use crate::queue::DeferredEntry;
use crate::time::{Timestamp, format_time};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkKind {
    /// A gate failed or could not be decided.
    Gate,
    /// Two claims conflict (automatic slot detection or opened through `wi_work`).
    Conflict,
    /// A `supersedes` target is invalid.
    InvalidSupersedes,
    /// A pinned source drifted, went missing or aged out.
    Recheck,
    /// A capped change waits in the deferred queue.
    Deferred,
    /// An agent proposed a rule or preference; only a user file edit can adopt it.
    OriginRule,
    /// A capture is still `pending`.
    PendingCapture,
    /// A procedure needs upkeep (lint, stale sources, duplicate `skill_id`,
    /// too large to deliver, excluded without replacement).
    Procedure,
    /// Repeated notes may describe a reusable procedure.
    PromoteCandidate,
    /// Notes share an exact normalized body of at least 200 characters.
    Duplicate,
}

impl WorkKind {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Gate => "gate",
            Self::Conflict => "conflict",
            Self::InvalidSupersedes => "invalid-supersedes",
            Self::Recheck => "recheck",
            Self::Deferred => "deferred",
            Self::OriginRule => "origin-rule",
            Self::PendingCapture => "pending-capture",
            Self::Procedure => "procedure",
            Self::PromoteCandidate => "promote-candidate",
            Self::Duplicate => "duplicate",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    Session,
    SourceChange,
    Query,
    Cooldown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Ready,
    Investigating,
    Checkpointed,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Adopted,
    Narrowed,
    Alternative,
    NotSupported,
    EvidenceUnavailable,
    ClosedWithoutSupportedAnswer,
    /// The cause is gone (the maintenance pass closes items whose condition
    /// no longer holds).
    Resolved,
}

impl Outcome {
    /// Every other outcome stays visible as unresolved and needs limits and a reopen trigger.
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            Self::Adopted | Self::Narrowed | Self::Alternative | Self::Resolved
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reopen {
    pub trigger: Trigger,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WorkError {
    #[error("outcome {0:?} needs limits and a concrete reopen trigger")]
    UnsupportedOutcomeNeedsLimits(Outcome),
    #[error("work item is already closed")]
    AlreadyClosed,
}

/// One work item. The id depends on kind, target and related notes, except
/// duplicate groups whose body digest remains stable as membership changes,
/// so a repeated sweep refreshes the same item instead of creating another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub kind: WorkKind,
    pub target: String,
    pub related: Vec<String>,
    pub state: State,
    pub reason: String,
    pub next_action: String,
    pub next_trigger: Trigger,
    pub outcome: Option<Outcome>,
    pub limits: Option<String>,
    pub reopen: Option<Reopen>,
    pub deferred: Option<DeferredEntry>,
    pub created_at: Timestamp,
    /// The observed state of the trigger condition (for example the hashes of
    /// a drifted source). Closure records it as `reopen.observed`, and a
    /// closed item reopens only when the condition is observed in another state.
    #[serde(default)]
    pub condition: Option<String>,
}

impl WorkItem {
    pub fn new(
        kind: WorkKind,
        target: impl Into<String>,
        related: Vec<String>,
        reason: impl Into<String>,
        next_action: impl Into<String>,
        next_trigger: Trigger,
        now: Timestamp,
    ) -> Self {
        let target = target.into();
        let mut related = related;
        related.sort();
        related.dedup();
        Self {
            id: work_id(kind, &target, &related),
            kind,
            target,
            related,
            state: State::Ready,
            reason: reason.into(),
            next_action: next_action.into(),
            next_trigger,
            outcome: None,
            limits: None,
            reopen: None,
            deferred: None,
            created_at: now,
            condition: None,
        }
    }

    pub fn with_condition(self, condition: impl Into<String>) -> Self {
        Self {
            condition: Some(condition.into()),
            ..self
        }
    }

    pub fn with_deferred(self, entry: DeferredEntry) -> Self {
        Self {
            deferred: Some(entry),
            ..self
        }
    }

    /// Vault-relative path for the work note.
    pub fn path(&self, work_dir: &str) -> String {
        format!(
            "{}/{}-{}.md",
            work_dir.trim_end_matches('/'),
            self.kind.code(),
            self.id
        )
    }

    pub fn start(&self) -> Self {
        Self {
            state: State::Investigating,
            ..self.clone()
        }
    }

    /// A stop for time or budget alone is a checkpoint, never a close.
    pub fn checkpoint(&self, next_action: impl Into<String>) -> Self {
        Self {
            state: State::Checkpointed,
            next_action: next_action.into(),
            ..self.clone()
        }
    }

    /// Honest closure: unsupported outcomes need `limits` and a `reopen` trigger.
    pub fn close(
        &self,
        outcome: Outcome,
        limits: Option<String>,
        reopen: Option<Reopen>,
    ) -> Result<Self, WorkError> {
        if self.state == State::Closed {
            return Err(WorkError::AlreadyClosed);
        }
        let limits = limits.filter(|l| !l.trim().is_empty());
        let reopen = reopen.filter(|r| !r.detail.trim().is_empty());
        if !outcome.is_success() && (limits.is_none() || reopen.is_none()) {
            return Err(WorkError::UnsupportedOutcomeNeedsLimits(outcome));
        }
        Ok(Self {
            state: State::Closed,
            outcome: Some(outcome),
            limits,
            reopen,
            ..self.clone()
        })
    }

    /// Only a matching trigger event restarts a closed item. `wi sweep` also
    /// requires the observed condition to differ from the one recorded at closure.
    pub fn reopen_on(&self, trigger: Trigger) -> Option<Self> {
        let matches = self.state == State::Closed
            && self.reopen.as_ref().is_some_and(|r| r.trigger == trigger);
        matches.then(|| Self {
            state: State::Ready,
            outcome: None,
            ..self.clone()
        })
    }

    /// Render as a `type: work` note. Values are JSON flow scalars, which
    /// YAML 1.1 and 1.2 loaders both read.
    pub fn render(&self) -> String {
        let work = json!({
            "target": self.target,
            "state": self.state,
            "next_action": self.next_action,
            "next_trigger": self.next_trigger,
            "outcome": self.outcome,
            "reason": self.reason,
            "limits": self.limits,
            "reopen": self.reopen.as_ref().map(|r| json!({
                "trigger": r.trigger,
                "detail": r.detail,
                "observed": self.condition,
            })),
            // The observed trigger condition, persisted from creation on so a
            // later closure records it as `reopen.observed`.
            "condition": self.condition,
        });
        let mut lines = vec![
            "---".to_string(),
            format!(
                "title: {}",
                json!(format!("{} {}", self.kind.code(), self.target))
            ),
            "type: work".to_string(),
            format!("work_id: {}", json!(self.id)),
            format!("work_kind: {}", json!(self.kind)),
            format!("related: {}", json!(self.related)),
            format!("created_at: {}", json!(format_time(self.created_at))),
            format!("work: {work}"),
        ];
        if let Some(entry) = &self.deferred {
            lines.push(format!("deferred: {}", json!(entry)));
        }
        lines.push("---".to_string());
        lines.push(String::new());
        lines.push(format!("{}\n", self.reason));
        lines.join("\n")
    }
}

/// `sha256(kind, target, related)` shortened to 16 hex characters. Duplicate
/// groups target a body digest and omit changing member paths from identity.
/// Legacy duplicate items targeting a note retain their original identity.
pub fn work_id(kind: WorkKind, target: &str, related: &[String]) -> String {
    let body_group = kind == WorkKind::Duplicate
        && target
            .strip_prefix("sha256:")
            .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()));
    let related = if body_group {
        String::new()
    } else {
        related.join("\0")
    };
    let key = format!("{}\0{}\0{}", kind.code(), target, related);
    sha256(key.as_bytes())["sha256:".len().."sha256:".len() + 16].to_string()
}
