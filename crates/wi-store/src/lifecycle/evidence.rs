//! Pinned-source evidence: current bytes, the engine-recorded time of the
//! pinned bytes, and caller-supplied observation events of URL snapshots.

use serde::{Deserialize, Serialize};
use wi_history::{PinRef, hash_bytes, resolve_pin};
use wi_lifecycle::{Evidence, NoteSnapshot, SourceState, Timestamp};
use wi_note::SourcePin;

use crate::contained::read_rel;
use crate::vault::Vault;

/// Suffix of the observation sidecar of a URL snapshot. It is written only
/// by the engine (ordinary tools write Markdown notes only).
pub(crate) const OBSERVED_SUFFIX: &str = ".wi-observed.json";

/// One supplied URL observation: when, the bytes hash, and ingress origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Observation {
    pub at: String,
    pub hash: String,
    #[serde(default = "unknown_observer")]
    pub observed_by: String,
}

fn unknown_observer() -> String {
    "unknown".into()
}

/// The sidecar of a URL snapshot: its URL and every observation event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Observed {
    pub url: String,
    pub observations: Vec<Observation>,
}

pub(crate) fn sidecar_path(snapshot: &str) -> String {
    format!("{snapshot}{OBSERVED_SUFFIX}")
}

fn millis(ms: i64) -> Option<Timestamp> {
    chrono::DateTime::from_timestamp_millis(ms)
}

impl Vault {
    /// The observation sidecar of a snapshot, if any.
    pub(crate) fn observed(&self, snapshot: &str) -> Option<Observed> {
        let bytes = read_rel(self.root(), &sidecar_path(snapshot)).ok()??;
        serde_json::from_slice(&bytes).ok()
    }

    /// When the engine recorded the pinned bytes: the manifest record time,
    /// or the author time of the commit holding them. Cached per pin.
    fn evidence_time(&self, pin: &SourcePin) -> Option<Timestamp> {
        let key = (
            pin.reference.clone(),
            pin.source_hash.clone(),
            pin.commit.clone(),
        );
        if let Some(time) = self.lifecycle_cache.borrow().evidence_times.get(&key) {
            return *time;
        }
        let resolved = resolve_pin(
            &self.history,
            &PinRef {
                path: pin.reference.clone(),
                source_hash: pin.source_hash.clone(),
                commit: pin.commit.clone(),
                pending: pin.pending.unwrap_or(pin.commit.is_none()),
            },
        )
        .ok()
        .and_then(|r| millis(r.recorded_ms));
        // An unresolved pin may resolve after the next flush: cache hits only.
        if resolved.is_some() {
            self.lifecycle_cache
                .borrow_mut()
                .evidence_times
                .insert(key, resolved);
        }
        resolved
    }

    fn source_state(&self, pin: &SourcePin) -> SourceState {
        let bytes = read_rel(self.root(), &pin.reference).ok().flatten();
        let is_url = pin.locator.as_ref().is_some_and(|l| l.url.is_some());
        if !is_url {
            return SourceState {
                bytes,
                evidence_time: self.evidence_time(pin),
                observations: Vec::new(),
                observed_by: Default::default(),
            };
        }
        // A URL snapshot: its capture time and supplied observations that
        // returned its current bytes. Re-hashing stored bytes is not one.
        let current = bytes.as_deref().map(hash_bytes);
        let events = self
            .observed(&pin.reference)
            .map(|o| o.observations)
            .unwrap_or_default();
        let times = |hash: Option<&str>| -> Vec<Timestamp> {
            events
                .iter()
                .filter(|e| Some(e.hash.as_str()) == hash)
                .filter_map(|e| wi_lifecycle::parse_time(&e.at).ok())
                .collect()
        };
        let captured = times(Some(pin.source_hash.as_str())).into_iter().min();
        let mut observed_by = std::collections::BTreeMap::<String, Vec<String>>::new();
        for event in &events {
            let observers = observed_by.entry(event.hash.clone()).or_default();
            observers.push(event.observed_by.clone());
            observers.sort();
            observers.dedup();
        }
        SourceState {
            bytes,
            evidence_time: captured.or_else(|| self.evidence_time(pin)),
            observations: times(current.as_deref()),
            observed_by,
        }
    }

    /// Evidence for every pin of `notes`.
    pub(crate) fn evidence_for<'a>(
        &self,
        notes: impl IntoIterator<Item = &'a NoteSnapshot>,
    ) -> Evidence {
        let mut evidence = Evidence::default();
        for note in notes {
            for pin in note.lifecycle.sources.iter().flatten() {
                if evidence.get(&pin.reference).is_none() {
                    let state = self.source_state(pin);
                    evidence = evidence.with(pin.reference.clone(), state);
                }
            }
        }
        evidence
    }
}
