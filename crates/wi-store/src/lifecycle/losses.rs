//! Durable loss events: unretired manifest intents and `Wi-Loss` commit
//! trailers. The change-cap window is rebuilt from them on every decision,
//! so it survives index loss and is shared by every process.

use std::collections::BTreeSet;

use wi_history::{History, Manifest, ManifestSnapshot, OpRecord, OpStatus};
use wi_lifecycle::{BudgetDecision, BudgetLedger, LossEvent, budget_check};

use crate::error::{Result, StoreError};
use crate::lifecycle::Charge;
use crate::vault::Vault;

/// Commits older than the window plus this margin cannot hold a live event.
const WINDOW_SECONDS: i64 = wi_lifecycle::budget::WINDOW_SECONDS;

impl Vault {
    fn losses_in(&self, snap: &ManifestSnapshot) -> Result<Vec<LossEvent>> {
        let now = self.lifecycle_now();
        let since = now.timestamp() - WINDOW_SECONDS;
        let mut events = BTreeSet::new();
        for entry in &snap.entries {
            if let OpRecord::Intent(rec) = &entry.op
                && matches!(entry.status, OpStatus::Done | OpStatus::Conflict { .. })
            {
                events.extend(
                    rec.losses
                        .iter()
                        .filter_map(|v| serde_json::from_value::<LossEvent>(v.clone()).ok()),
                );
            }
        }
        for value in History::losses_since(&self.history, since)? {
            if let Ok(event) = serde_json::from_str::<LossEvent>(&value) {
                events.insert(event);
            }
        }
        Ok(events.into_iter().collect())
    }

    /// Every durable loss event that may still count (outside the lock).
    pub(crate) fn durable_losses(&self) -> Result<Vec<LossEvent>> {
        let snap = Manifest::snapshot_unlocked(&self.history)?;
        self.losses_in(&snap)
    }

    /// The ledger of durable loss events.
    pub(crate) fn loss_ledger(&self) -> Result<BudgetLedger> {
        Ok(BudgetLedger::new(self.durable_losses()?))
    }

    /// Re-check a charge against the durable events under the publisher
    /// lock. Allowed: the events to record in the intent. Spent: refused.
    pub(crate) fn charge_under_lock(
        &self,
        snap: &ManifestSnapshot,
        charge: &Charge,
    ) -> Result<Vec<serde_json::Value>> {
        if charge.events.is_empty() {
            return Ok(Vec::new());
        }
        let durable = self.losses_in(snap)?;
        let now = self.lifecycle_now();
        match budget_check(&charge.requests, &durable, &charge.counts, now) {
            BudgetDecision::Allowed => Ok(charge
                .events
                .iter()
                .filter_map(|e| serde_json::to_value(e).ok())
                .collect()),
            BudgetDecision::BudgetExhausted(exhausted) => Err(StoreError::BudgetExhausted {
                retry_at: exhausted.retry_at.map(wi_lifecycle::format_time),
                spent: exhausted.spent.len(),
                work: None,
            }),
        }
    }
}
