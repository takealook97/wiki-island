//! The automatic-change cap: a per-entity trailing 24-hour window over
//! durably recorded visibility-loss events.

use std::collections::BTreeMap;

use chrono::TimeDelta;
use serde::{Deserialize, Serialize};

use crate::conflict::competes;
use crate::model::{Actor, Policy, Snapshot};
use crate::time::Timestamp;

/// The trailing window length.
pub const WINDOW_SECONDS: i64 = 24 * 3600;
/// Per-entity budget divisor: a third of the entity's claims.
pub const ENTITY_DIVISOR: usize = 3;

fn window() -> TimeDelta {
    TimeDelta::seconds(WINDOW_SECONDS)
}

/// An action that removes a current claim from current reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LossKind {
    Supersede,
    Archive,
    /// `wi_move` into the archive directory, charged like `wi_archive`.
    MoveToArchive,
    Exclude,
    Conflict,
}

/// One requested loss: a target and its entities before the change.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LossRequest {
    pub target: String,
    pub entities: Vec<String>,
    pub kind: LossKind,
}

/// A durably recorded loss (manifest record and commit trailer).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LossEvent {
    pub at: Timestamp,
    pub target: String,
    pub entities: Vec<String>,
    pub kind: LossKind,
    pub actor: Actor,
}

impl LossEvent {
    pub fn from_request(request: &LossRequest, actor: Actor, at: Timestamp) -> Self {
        Self {
            at,
            target: request.target.clone(),
            entities: request.entities.clone(),
            kind: request.kind,
            actor,
        }
    }

    /// Counts while `now < at + 24h`. An event stamped after `now`
    /// (the clock moved backward) therefore still counts.
    pub fn in_window(&self, now: Timestamp) -> bool {
        self.at
            .checked_add_signed(window())
            .is_none_or(|end| now < end)
    }

    fn expires_at(&self) -> Option<Timestamp> {
        self.at.checked_add_signed(window())
    }
}

/// Active claim counts at the time of the request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveCounts {
    pub per_entity: BTreeMap<String, usize>,
    pub total: usize,
}

impl ActiveCounts {
    pub fn from_snapshot(snapshot: &Snapshot, policy: &Policy) -> Self {
        let active: Vec<_> = snapshot
            .notes()
            .iter()
            .filter(|n| competes(n, policy))
            .collect();
        let mut per_entity = BTreeMap::new();
        for id in active.iter().flat_map(|n| n.entity_ids()) {
            *per_entity.entry(id).or_insert(0) += 1;
        }
        Self {
            per_entity,
            total: active.len(),
        }
    }

    /// Counts after the requested losses applied.
    pub fn after(&self, requests: &[LossRequest]) -> Self {
        let mut per_entity = self.per_entity.clone();
        for id in requests.iter().flat_map(|r| &r.entities) {
            if let Some(n) = per_entity.get_mut(id) {
                *n = n.saturating_sub(1);
            }
        }
        Self {
            per_entity,
            total: self.total.saturating_sub(requests.len()),
        }
    }
}

/// `max(1, floor((active_now + losses_in_window) / 3))`.
pub fn entity_cap(active_now: usize, losses_in_window: usize) -> usize {
    ((active_now + losses_in_window) / ENTITY_DIVISOR).max(1)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "scope", content = "id")]
pub enum BudgetScope {
    Entity(String),
}

/// The `budget-exhausted` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetExhausted {
    /// Earliest time the action fits if nothing else is spent; `None` if it
    /// never fits automatically (for example more losses than the cap).
    pub retry_at: Option<Timestamp>,
    /// The in-window operations that spent the exhausted budgets.
    pub spent: Vec<LossEvent>,
    pub scopes: Vec<BudgetScope>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "decision")]
pub enum BudgetDecision {
    Allowed,
    BudgetExhausted(BudgetExhausted),
}

/// One budget scope: in-window events, requested losses and its cap function.
struct Scope<'a> {
    scope: BudgetScope,
    events: Vec<&'a LossEvent>,
    requested: usize,
    cap: Box<dyn Fn(usize) -> usize + 'a>,
}

impl Scope<'_> {
    fn fits(&self, losses: usize) -> bool {
        losses + self.requested <= (self.cap)(losses)
    }

    /// Expire events in order until the request fits.
    fn retry_at(&self) -> Option<Timestamp> {
        let mut expiries: Vec<Option<Timestamp>> =
            self.events.iter().map(|e| e.expires_at()).collect();
        expiries.sort();
        (1..=expiries.len())
            .find(|dropped| self.fits(expiries.len() - dropped))
            .and_then(|dropped| expiries[dropped - 1])
    }
}

/// Check whether `requests` fit the cap of every entity they touch at
/// `now`. Pure and order-independent.
pub fn budget_check(
    requests: &[LossRequest],
    events: &[LossEvent],
    counts: &ActiveCounts,
    now: Timestamp,
) -> BudgetDecision {
    if requests.is_empty() {
        return BudgetDecision::Allowed;
    }
    let live: Vec<&LossEvent> = events.iter().filter(|e| e.in_window(now)).collect();
    let mut requested: BTreeMap<&str, usize> = BTreeMap::new();
    for id in requests.iter().flat_map(|r| &r.entities) {
        *requested.entry(id.as_str()).or_insert(0) += 1;
    }
    let scopes: Vec<Scope> = requested
        .iter()
        .map(|(id, k)| {
            let active = counts.per_entity.get(*id).copied().unwrap_or(0);
            Scope {
                scope: BudgetScope::Entity(id.to_string()),
                events: live
                    .iter()
                    .copied()
                    .filter(|e| e.entities.iter().any(|x| x == id))
                    .collect(),
                requested: *k,
                cap: Box::new(move |losses| entity_cap(active, losses)),
            }
        })
        .collect();
    let failing: Vec<&Scope> = scopes.iter().filter(|s| !s.fits(s.events.len())).collect();
    if failing.is_empty() {
        return BudgetDecision::Allowed;
    }
    let retry_at = failing
        .iter()
        .map(|s| s.retry_at())
        .collect::<Option<Vec<_>>>()
        .and_then(|times| times.into_iter().max());
    let mut spent: Vec<LossEvent> = failing
        .iter()
        .flat_map(|s| s.events.iter().map(|e| (*e).clone()))
        .collect();
    spent.sort();
    spent.dedup();
    BudgetDecision::BudgetExhausted(BudgetExhausted {
        retry_at,
        spent,
        scopes: failing.iter().map(|s| s.scope.clone()).collect(),
    })
}

/// The loss events a publisher reserves under its lock. Immutable: every
/// reservation returns a new ledger. The host persists the new events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetLedger {
    events: Vec<LossEvent>,
}

impl BudgetLedger {
    pub fn new(events: Vec<LossEvent>) -> Self {
        let mut events = events;
        events.sort();
        Self { events }
    }

    pub fn events(&self) -> &[LossEvent] {
        &self.events
    }

    pub fn check(
        &self,
        requests: &[LossRequest],
        counts: &ActiveCounts,
        now: Timestamp,
    ) -> BudgetDecision {
        budget_check(requests, &self.events, counts, now)
    }

    /// Check and record in one step. User actions are not capped and not recorded.
    pub fn reserve(
        &self,
        requests: &[LossRequest],
        actor: Actor,
        counts: &ActiveCounts,
        now: Timestamp,
    ) -> Result<(Self, Vec<LossEvent>), BudgetExhausted> {
        if actor == Actor::User {
            return Ok((self.clone(), Vec::new()));
        }
        if let BudgetDecision::BudgetExhausted(exhausted) = self.check(requests, counts, now) {
            return Err(exhausted);
        }
        let added: Vec<LossEvent> = requests
            .iter()
            .map(|r| LossEvent::from_request(r, actor, now))
            .collect();
        let events = self.events.iter().cloned().chain(added.clone()).collect();
        Ok((Self::new(events), added))
    }

    /// Drop events that can never count again (`now >= at + 24h`).
    pub fn prune(&self, now: Timestamp) -> Self {
        Self::new(
            self.events
                .iter()
                .filter(|e| e.in_window(now))
                .cloned()
                .collect(),
        )
    }
}
