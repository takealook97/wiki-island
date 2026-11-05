//! Read-time freshness (KNOWLEDGE "Freshness"). Nothing here is stored as status.

use serde::Serialize;
use wi_note::{CheckResult, ClaimKind, Lifecycle, OriginKind, SourcePin};

use crate::model::{Evidence, Policy};
use crate::time::{IsoDuration, TimeError, Timestamp, parse_time};

/// Why a claim is or is not citable right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "state", content = "detail")]
pub enum FreshnessState {
    /// Within its window.
    Fresh,
    /// `historical` and `intent` claims do not age out.
    Timeless,
    /// `now >= last_verified + stale_after_days`.
    Stale,
    /// `now >= last_verified + ttl`.
    TtlExpired,
    /// A pinned source's bytes changed since the pin.
    Drifted(Vec<String>),
    /// A pinned vault source is missing.
    SourceMissing(Vec<String>),
    /// A URL pin has no local snapshot.
    NoSnapshot(Vec<String>),
    /// An observed, checked or observation time lies after `now`.
    FutureTimestamp(String),
    /// A current-state claim without `observed_at`.
    MissingObservedAt,
    /// A non-user claim pins sources whose engine-recorded evidence time is unknown.
    MissingEvidenceTime(Vec<String>),
    /// A timestamp or `ttl` that cannot be parsed.
    Unparseable(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Freshness {
    pub state: FreshnessState,
    pub effective_observed_at: Option<Timestamp>,
    pub last_verified: Option<Timestamp>,
    /// Exclusive end of the citable window, when the claim ages.
    pub citable_until: Option<Timestamp>,
}

impl Freshness {
    pub fn citable(&self) -> bool {
        matches!(self.state, FreshnessState::Fresh | FreshnessState::Timeless)
    }

    fn of(state: FreshnessState) -> Self {
        Self {
            state,
            effective_observed_at: None,
            last_verified: None,
            citable_until: None,
        }
    }
}

pub(crate) fn is_url_pin(pin: &SourcePin) -> bool {
    pin.locator.as_ref().is_some_and(|l| l.url.is_some())
}

fn pins(lifecycle: &Lifecycle) -> &[SourcePin] {
    lifecycle.sources.as_deref().unwrap_or_default()
}

fn opt_time(text: Option<&str>) -> Result<Option<Timestamp>, TimeError> {
    text.map(parse_time).transpose()
}

pub(crate) fn is_user_origin(lifecycle: &Lifecycle) -> bool {
    lifecycle
        .origin
        .as_ref()
        .is_some_and(|o| o.kind == OriginKind::User)
}

/// Pins of a non-user claim whose engine-recorded evidence time is unknown.
/// A user-origin claim keeps its own time and needs none.
pub fn missing_evidence_times(lifecycle: &Lifecycle, evidence: &Evidence) -> Vec<String> {
    if is_user_origin(lifecycle) {
        return Vec::new();
    }
    pins(lifecycle)
        .iter()
        .filter(|pin| {
            evidence
                .get(&pin.reference)
                .and_then(|s| s.evidence_time)
                .is_none()
        })
        .map(|pin| pin.reference.clone())
        .collect()
}

/// For a user-origin note, the claimed `observed_at`. For any other origin
/// (agent, file, url or unknown) the claim is lowered to the earliest
/// engine-recorded evidence time of its pinned sources. If any pin's
/// evidence time is unknown, the effective time is unknown (`None`): the
/// claimed time is never trusted in its place.
pub fn effective_observed_at(
    lifecycle: &Lifecycle,
    evidence: &Evidence,
) -> Result<Option<Timestamp>, TimeError> {
    let Some(claimed) = opt_time(lifecycle.observed_at.as_deref())? else {
        return Ok(None);
    };
    if is_user_origin(lifecycle) {
        return Ok(Some(claimed));
    }
    let times: Option<Vec<Timestamp>> = pins(lifecycle)
        .iter()
        .map(|pin| evidence.get(&pin.reference)?.evidence_time)
        .collect();
    Ok(times.map(|t| t.into_iter().min().map_or(claimed, |e| e.min(claimed))))
}

/// The latest observation shared by every pin, or `None` when some pin was never observed.
fn pin_observation(
    lifecycle: &Lifecycle,
    evidence: &Evidence,
    now: Timestamp,
) -> Result<Option<Timestamp>, TimeError> {
    let check = lifecycle.check.as_ref();
    let internal = match check {
        Some(c) if c.result == Some(CheckResult::Unchanged) => opt_time(c.checked_at.as_deref())?,
        _ => None,
    };
    let pins = pins(lifecycle);
    if pins.is_empty() {
        return Ok(None);
    }
    let per_pin = pins.iter().map(|pin| {
        if is_url_pin(pin) {
            evidence
                .get(&pin.reference)
                .and_then(|s| s.observations.iter().copied().filter(|t| *t <= now).max())
        } else {
            internal
        }
    });
    Ok(per_pin
        .collect::<Option<Vec<_>>>()
        .and_then(|v| v.into_iter().min()))
}

fn future_timestamp(
    lifecycle: &Lifecycle,
    evidence: &Evidence,
    now: Timestamp,
) -> Result<Option<String>, TimeError> {
    if opt_time(lifecycle.observed_at.as_deref())?.is_some_and(|t| t > now) {
        return Ok(Some("observed_at".into()));
    }
    if let Some(check) = &lifecycle.check {
        if opt_time(check.checked_at.as_deref())?.is_some_and(|t| t > now) {
            return Ok(Some("check.checked_at".into()));
        }
        if opt_time(check.integrity_at.as_deref())?.is_some_and(|t| t > now) {
            return Ok(Some("check.integrity_at".into()));
        }
    }
    let future_observation = pins(lifecycle).iter().find(|pin| {
        evidence
            .get(&pin.reference)
            .is_some_and(|s| s.observations.iter().any(|t| *t > now))
    });
    Ok(future_observation.map(|pin| format!("observation of {}", pin.reference)))
}

/// Missing, snapshot-less and drifted pins, in that order of precedence.
fn source_problem(lifecycle: &Lifecycle, evidence: &Evidence) -> Option<FreshnessState> {
    let mut missing = Vec::new();
    let mut no_snapshot = Vec::new();
    let mut drifted = Vec::new();
    for pin in pins(lifecycle) {
        match evidence.get(&pin.reference).and_then(|s| s.hash()) {
            None if is_url_pin(pin) => no_snapshot.push(pin.reference.clone()),
            None => missing.push(pin.reference.clone()),
            Some(hash) if hash != pin.source_hash => drifted.push(pin.reference.clone()),
            Some(_) => {}
        }
    }
    if !no_snapshot.is_empty() {
        Some(FreshnessState::NoSnapshot(no_snapshot))
    } else if !missing.is_empty() {
        Some(FreshnessState::SourceMissing(missing))
    } else if !drifted.is_empty() {
        Some(FreshnessState::Drifted(drifted))
    } else {
        None
    }
}

/// The aging window and whether it is a `ttl`: `ttl` for volatile or
/// URL-pinned claims (default `P7D` when none is recorded), else `stale_after_days`.
fn window(lifecycle: &Lifecycle, policy: &Policy) -> Result<(IsoDuration, bool), TimeError> {
    let check = lifecycle.check.as_ref();
    if let Some(ttl) = check.and_then(|c| c.ttl.as_deref()) {
        return Ok((IsoDuration::parse(ttl)?, true));
    }
    let volatile = check.and_then(|c| c.volatile).unwrap_or(false);
    if volatile || pins(lifecycle).iter().any(is_url_pin) {
        return Ok((policy.url_default_ttl, true));
    }
    Ok((IsoDuration::days(policy.stale_after_days), false))
}

/// `last_verified = max(effective_observed_at, latest observation of the pinned sources)`.
pub fn last_verified(
    lifecycle: &Lifecycle,
    evidence: &Evidence,
    now: Timestamp,
) -> Result<Option<Timestamp>, TimeError> {
    let effective = effective_observed_at(lifecycle, evidence)?;
    let observed = pin_observation(lifecycle, evidence, now)?;
    Ok(effective.map(|e| observed.map_or(e, |o| e.max(o))))
}

/// Compute freshness at `now`. Status is never consulted or changed.
pub fn freshness(
    lifecycle: &Lifecycle,
    evidence: &Evidence,
    policy: &Policy,
    now: Timestamp,
) -> Freshness {
    match compute(lifecycle, evidence, policy, now) {
        Ok(freshness) => freshness,
        Err(error) => Freshness::of(FreshnessState::Unparseable(error.to_string())),
    }
}

fn compute(
    lifecycle: &Lifecycle,
    evidence: &Evidence,
    policy: &Policy,
    now: Timestamp,
) -> Result<Freshness, TimeError> {
    if let Some(field) = future_timestamp(lifecycle, evidence, now)? {
        return Ok(Freshness::of(FreshnessState::FutureTimestamp(field)));
    }
    let effective = effective_observed_at(lifecycle, evidence)?;
    let verified = last_verified(lifecycle, evidence, now)?;
    let with = |state, until| Freshness {
        state,
        effective_observed_at: effective,
        last_verified: verified,
        citable_until: until,
    };
    if let Some(problem) = source_problem(lifecycle, evidence) {
        return Ok(with(problem, None));
    }
    let unknown = missing_evidence_times(lifecycle, evidence);
    if !unknown.is_empty() {
        return Ok(with(FreshnessState::MissingEvidenceTime(unknown), None));
    }
    // Engine-origin text is the binary's own; it changes with the binary.
    let engine = lifecycle
        .origin
        .as_ref()
        .is_some_and(|o| o.kind == OriginKind::Engine);
    if engine
        || matches!(
            lifecycle.claim_kind,
            Some(ClaimKind::Historical) | Some(ClaimKind::Intent)
        )
    {
        return Ok(with(FreshnessState::Timeless, None));
    }
    let Some(verified) = verified else {
        return Ok(with(FreshnessState::MissingObservedAt, None));
    };
    let (window, uses_ttl) = window(lifecycle, policy)?;
    let until = window
        .after(verified)
        .ok_or_else(|| TimeError::Duration("window overflow".into()))?;
    let state = if now < until {
        FreshnessState::Fresh
    } else if uses_ttl {
        FreshnessState::TtlExpired
    } else {
        FreshnessState::Stale
    };
    Ok(with(state, Some(until)))
}
