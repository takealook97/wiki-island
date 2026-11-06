//! Bounded candidate windows for paged search and metadata find.
//!
//! A page needs `offset + limit` accepted candidates, not every match. The
//! window asks the index for a small multiple of that, ranked and limited in
//! SQL, and grows it only while a post-filter (observation categories, a
//! folder prefix) or hash verification leaves the page short. Totals come
//! from a capped `COUNT` when no post-filter applies.

use serde_json::{Value, json};
use wi_store::{Query, SearchHit, SearchOptions, SearchResponse, StoreError, Vault};

use crate::result;

/// Most candidates one request may examine (deep offsets stop here).
pub(crate) const WINDOW_MAX: usize = 10_000;
/// Matching notes counted for `total`; beyond it `total_capped` is true.
pub(crate) const TOTAL_CAP: usize = 1_000;
/// First window as a multiple of the candidates a page needs.
const OVERFETCH: usize = 4;
/// Window growth rounds (each doubles the window).
const MAX_ROUNDS: usize = 8;

/// Accepted candidates for one page and the observed totals.
pub(crate) struct Window {
    /// The last index response (withheld, diagnostics, freshness).
    pub response: SearchResponse,
    /// Accepted candidates in rank order, at least `offset + limit` when
    /// that many exist within the window.
    pub accepted: Vec<SearchHit>,
    /// Observed total: exact unless `capped`.
    pub total: usize,
    /// More matches may exist beyond `total`.
    pub capped: bool,
    /// The window reached [`WINDOW_MAX`] with further matches unexamined.
    pub truncated: bool,
}

/// Fill a window for the page `offset..offset + limit`. `accept` is the
/// post-filter; `None` accepts every hit and lets the index count totals.
pub(crate) fn fill(
    vault: &Vault,
    mut query: Query,
    offset: usize,
    limit: usize,
    mut accept: Option<&mut dyn FnMut(&SearchHit) -> bool>,
) -> Result<Window, StoreError> {
    let want = offset.saturating_add(limit).clamp(1, WINDOW_MAX);
    let mut size = want.saturating_mul(OVERFETCH).min(WINDOW_MAX);
    query.count_limit = if accept.is_none() { TOTAL_CAP } else { 0 };
    let mut counted = None;
    for round in 1..=MAX_ROUNDS {
        query.limit = size;
        let response = vault.search(&query, SearchOptions::default())?;
        counted = counted.or(response.matches);
        query.count_limit = 0;
        let exhausted = response.examined < size;
        let accepted: Vec<SearchHit> = match accept.as_mut() {
            Some(f) => response.hits.iter().filter(|h| f(h)).cloned().collect(),
            None => response.hits.clone(),
        };
        let last = accepted.len() >= want || exhausted || size == WINDOW_MAX || round == MAX_ROUNDS;
        if !last {
            size = size.saturating_mul(2).min(WINDOW_MAX);
            continue;
        }
        let (total, capped) = match counted {
            Some(m) if m > TOTAL_CAP => (TOTAL_CAP.max(accepted.len()), true),
            Some(m) => (m.max(accepted.len()), false),
            None => (accepted.len(), !exhausted),
        };
        return Ok(Window {
            truncated: size == WINDOW_MAX && !exhausted,
            response,
            accepted,
            total,
            capped,
        });
    }
    unreachable!("the last round returns")
}

impl Window {
    /// Envelope fields shared by search and metadata find, then the page.
    pub(crate) fn page(self, offset: usize, limit: usize, out: &mut Value) -> Vec<SearchHit> {
        out["index_as_of"] = json!(self.response.index_as_of);
        out["fallback"] = json!(self.response.fallback);
        out["diagnostics"] = json!(self.response.diagnostics);
        for w in &self.response.withheld {
            result::withhold(out, &w.reason, w.count);
        }
        out["window_truncated"] = json!(self.truncated);
        out["total_capped"] = json!(self.capped);
        out["offset"] = json!(offset);
        out["total"] = json!(self.total);
        // Past a capped total, a full page means more may follow.
        let end = offset.saturating_add(limit);
        out["next_offset"] = if end < self.total || (self.capped && self.accepted.len() >= end) {
            json!(end)
        } else {
            Value::Null
        };
        result::withhold(
            out,
            "hit-limit",
            self.total.saturating_sub(offset).saturating_sub(limit),
        );
        self.accepted.into_iter().skip(offset).take(limit).collect()
    }
}
