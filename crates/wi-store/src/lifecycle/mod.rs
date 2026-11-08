//! Lifecycle wiring: the pure `wi-lifecycle` engine applied to this vault
//! (KNOWLEDGE, OPERATIONS `#work-items`).
//!
//! The engine decides; this module gathers its inputs (lifecycle notes,
//! pinned-source evidence, durable loss events, `now`) and applies its
//! outputs through the normal write path with the `engine` actor. Loss
//! events are charged under the publisher lock and recorded in the intent
//! record, so they reach commits as `Wi-Loss` trailers.

mod adopt;
mod agent;
mod apply;
mod capture;
mod deferred;
mod evidence;
mod losses;
mod ownership;
mod reads;
mod revise;
mod snapshot;
mod worknotes;

use std::cell::Cell;

use wi_lifecycle::{ActiveCounts, Actor, Engine, LossEvent, LossRequest, Policy, Timestamp};

pub use adopt::{AdoptReport, SweepSummary};
pub(crate) use agent::rule_kind;
pub use agent::{NOTED_PREFERENCES_DIR, NOTED_RULES_DIR, NotedNote, SPAN_MAX_BYTES};
pub use capture::{CaptureInput, Proposal, SnapshotInput};
pub use reads::{Classified, CurrentSearch, LintReport, ProcedureMode, RuleNote, RuleSet};
pub(crate) use snapshot::Cache;
pub use worknotes::{WorkClose, WorkEntry};

use crate::vault::{Ingress, Vault};

/// Environment variable fixing the lifecycle clock (RFC 3339) for
/// subprocess tests. Unset in normal operation.
pub const NOW_ENV: &str = "WI_NOW";

/// Archive directories configured on this device (device-local).
/// Inside the per-device local directory ([`Vault::local_dir`]).
const ARCHIVE_DIRS_FILE: &str = "archive-dirs.json";

/// A change-cap charge: the requests, who acts, the active counts before
/// the change and the events to record when the budget allows.
#[derive(Debug, Clone)]
pub struct Charge {
    pub requests: Vec<LossRequest>,
    pub actor: Actor,
    pub counts: ActiveCounts,
    pub events: Vec<LossEvent>,
}

/// The lifecycle actor for an ingress: MCP is an agent, the engine is the
/// engine, every file or CLI ingress is the user.
pub(crate) fn actor_of(ingress: Ingress) -> Actor {
    match ingress {
        Ingress::Mcp => Actor::Agent,
        Ingress::Engine => Actor::Engine,
        Ingress::Cli | Ingress::Watcher | Ingress::Import => Actor::User,
    }
}

impl Vault {
    /// The engine for this vault's policy (`lifecycle`, directories, ages).
    pub fn lifecycle_engine(&self) -> Engine {
        Engine::new(self.lifecycle_policy())
    }

    /// This vault's policy, with the archive directories it used before.
    pub(crate) fn lifecycle_policy(&self) -> Policy {
        Policy::from_config(&self.config).with_former_archive_dirs(self.former_archive_dirs.clone())
    }

    /// Remember the configured archive directory in device-local state and
    /// return the ones used before it: the default `archive` and every
    /// directory an earlier `archive_dir` named on this device. Notes a
    /// path-only archive put there stay archived when the key changes.
    /// Best effort: an unwritable state file only forgets a custom
    /// directory the next time the key changes.
    pub(crate) fn record_archive_dir(&self) -> Vec<String> {
        let file = self.local_dir().join(ARCHIVE_DIRS_FILE);
        let mut seen: Vec<String> = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let current = self.config.archive_dir().to_owned();
        if current != wi_note::ARCHIVE_DIR && !seen.contains(&current) {
            seen.push(current.clone());
            if let Ok(text) = serde_json::to_vec(&seen)
                && let Some(dir) = file.parent()
                && std::fs::create_dir_all(dir).is_ok()
            {
                let _ = wi_history::durable::write_atomic(&file, &text, self.history.durability());
            }
        }
        std::iter::once(wi_note::ARCHIVE_DIR.to_owned())
            .chain(seen)
            .filter(|dir| *dir != current)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Whether `lifecycle: off` is not set.
    pub fn lifecycle_on(&self) -> bool {
        self.config.lifecycle() != "off"
    }

    /// Lifecycle `now`: the test clock, else `WI_NOW`, else the system clock.
    pub fn lifecycle_now(&self) -> Timestamp {
        if let Some(now) = self.clock.get() {
            return now;
        }
        std::env::var(NOW_ENV)
            .ok()
            .and_then(|t| wi_lifecycle::parse_time(&t).ok())
            .unwrap_or_else(chrono::Utc::now)
    }

    /// Fix the lifecycle clock (tests). `None` restores the default.
    pub fn set_lifecycle_clock(&self, now: Option<Timestamp>) {
        Cell::set(&self.clock, now);
    }
}
