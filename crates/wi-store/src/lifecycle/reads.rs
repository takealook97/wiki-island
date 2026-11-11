//! Current-mode eligibility for reads, search, rules and procedures.

use serde::Serialize;
use serde_json::{Value, json};
use wi_index::{Query, SearchHit};
use wi_lifecycle::{Context, Eligibility, Engine, NoteSnapshot, ReadQuery, Snapshot, format_time};
use wi_note::{Kind, LifecycleType};

use crate::error::{Result, StoreError};
use crate::read::{GENERATION_CHANGED, SearchOptions, SearchResponse, Withheld};
use crate::vault::Vault;

/// One note judged for a `current` read.
#[derive(Debug, Clone, Serialize)]
pub struct Classified {
    pub path: String,
    /// `none` (plain), `memory`, `capture` or `work`.
    pub lifecycle: String,
    /// `current`, `plain` or `withheld`.
    pub eligibility: String,
    /// Reason code when withheld (no content).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Computed freshness for memory notes (`state`, `last_verified`, ...).
    pub freshness: Value,
    /// Source spans delivered with a non-literal claim.
    pub spans: Vec<wi_lifecycle::AttachedSpan>,
}

impl Classified {
    pub fn is_deliverable(&self) -> bool {
        self.eligibility != "withheld"
    }
}

/// A current-mode search: delivered hits plus withheld counts by reason.
#[derive(Debug, Clone, Serialize)]
pub struct CurrentSearch {
    pub response: SearchResponse,
    pub labels: Vec<Classified>,
}

/// A current rule or preference (`origin.kind=user`), for the export.
#[derive(Debug, Clone, Serialize)]
pub struct RuleNote {
    pub path: String,
    pub kind: String,
    pub summary: Option<String>,
    pub content: String,
    /// Earliest applicability or freshness expiry, when set.
    pub valid_until: Option<String>,
    pub spans: Vec<wi_lifecycle::AttachedSpan>,
}

/// Current rules and preferences plus the omitted ones with a reason.
#[derive(Debug, Clone, Serialize)]
pub struct RuleSet {
    pub rules: Vec<RuleNote>,
    /// (path, reason code) of rule or preference notes that are not current.
    pub omitted: Vec<(String, String)>,
    /// Earliest `valid_until` among the delivered rules.
    pub earliest_valid_until: Option<String>,
    pub generation: u64,
}

/// Procedure listing mode for skills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcedureMode {
    /// Only current, non-excluded procedures.
    Discovery,
    /// Every procedure note, each with its eligibility label, so a direct
    /// lookup can say why one is withheld or excluded.
    Direct,
}

fn lifecycle_name(t: LifecycleType) -> &'static str {
    match t {
        LifecycleType::Memory => "memory",
        LifecycleType::Capture => "capture",
        LifecycleType::Work => "work",
        LifecycleType::None => "none",
    }
}

/// Query conditions from a JSON object (`null` or absent: none).
pub(crate) fn read_query(conditions: &Value) -> Result<ReadQuery> {
    match conditions {
        Value::Null => Ok(ReadQuery::default()),
        Value::Object(map) => Ok(map.iter().fold(ReadQuery::default(), |q, (k, v)| {
            q.with(k.clone(), v.clone())
        })),
        _ => Err(StoreError::Invalid("conditions must be an object".into())),
    }
}

fn classify_one(engine: &Engine, note: &NoteSnapshot, ctx: &Context) -> Classified {
    let (eligibility, reason) = match engine.eligibility(note, ctx) {
        Eligibility::Current => ("current", None),
        Eligibility::Plain { .. } => ("plain", None),
        Eligibility::Withheld(w) => ("withheld", Some(w.code().to_owned())),
    };
    let memory = note.is_memory() && engine.policy.lifecycle_on;
    let freshness = if memory {
        let mut value = json!(engine.freshness(note, &ctx.snapshot.evidence, ctx.now));
        // Include provenance even for literal claims, which carry no attached spans.
        let sources: Vec<_> = note
            .lifecycle
            .sources
            .iter()
            .flatten()
            .filter(|pin| pin.locator.as_ref().is_some_and(|l| l.url.is_some()))
            .map(|pin| {
                json!({
                    "ref": pin.reference,
                    "source_hash": pin.source_hash,
                    "observed_by": ctx.snapshot.evidence.get(&pin.reference)
                        .map(|s| s.observers(&pin.source_hash))
                        .unwrap_or_else(|| vec!["unknown".to_owned()]),
                })
            })
            .collect();
        if !sources.is_empty() {
            value["sources"] = json!(sources);
        }
        value
    } else {
        json!("not-applicable")
    };
    let spans = if memory && eligibility == "current" {
        engine.attached_spans(note, ctx)
    } else {
        Vec::new()
    };
    Classified {
        path: note.path.clone(),
        lifecycle: lifecycle_name(note.lifecycle.lifecycle_type).into(),
        eligibility: eligibility.into(),
        reason,
        freshness,
        spans,
    }
}

impl Vault {
    /// Judge notes (path and current bytes) for a `current` read. With
    /// `lifecycle: off` every note is plain.
    pub fn classify(
        &self,
        notes: &[(String, Vec<u8>)],
        conditions: &Value,
    ) -> Result<Vec<Classified>> {
        let query = read_query(conditions)?;
        let fresh: Vec<NoteSnapshot> = notes
            .iter()
            .map(|(p, b)| NoteSnapshot::from_bytes(p.as_str(), b))
            .collect();
        let engine = self.lifecycle_engine();
        if !engine.policy.lifecycle_on {
            let ctx_snapshot = Snapshot::default();
            let ctx = Context::new(&ctx_snapshot, self.lifecycle_now());
            return Ok(fresh
                .iter()
                .map(|n| classify_one(&engine, n, &ctx))
                .collect());
        }
        let evidence = self.evidence_for(fresh.iter().filter(|n| n.is_memory()));
        let now = self.lifecycle_now();
        self.with_read_snapshot(evidence, |snapshot| {
            let ctx = Context::new(snapshot, now).with_query(query);
            fresh
                .iter()
                .map(|n| classify_one(&engine, n, &ctx))
                .collect()
        })
    }

    /// A `current` search: index search, re-read of every hit, eligibility
    /// with evidence read in the same generation-checked attempt. A pinned
    /// source or superseder that changes during the read changes the
    /// generation: the read retries once and judges again; a second change
    /// adds `generation-changed`.
    pub fn search_current(
        &self,
        query: &Query,
        conditions: &Value,
        options: SearchOptions,
    ) -> Result<CurrentSearch> {
        let mut last = None;
        for attempt in 0..2u32 {
            let start = self.publication_state()?;
            let response = self.search(query, options)?;
            let read: Vec<(String, Vec<u8>)> = response
                .hits
                .iter()
                .filter_map(|h| {
                    let path = h.rel_path.to_string_lossy().into_owned();
                    let bytes = crate::contained::read_rel(self.root(), &path).ok()??;
                    Some((path, bytes))
                })
                .collect();
            let labels = self.classify(&read, conditions)?;
            let end = self.publication_state()?;
            let stable = start == end && end.1.is_none();
            last = Some((response, labels));
            if stable {
                break;
            }
            if attempt == 1
                && let Some((response, _)) = &mut last
            {
                response.diagnostics.push(GENERATION_CHANGED.to_owned());
            }
        }
        let Some((response, labels)) = last else {
            return Err(StoreError::Invalid("search produced no attempt".into()));
        };
        Ok(filter_response(response, labels))
    }

    /// Current rules and preferences (`origin.kind=user`), with the omitted
    /// ones and the earliest `valid_until`. Interface for export.
    pub fn current_rules(&self) -> Result<RuleSet> {
        let generation = self.generation()?;
        let notes = self.lifecycle_notes_where(|n| n.is_memory() && n.is_rule_like())?;
        let rule_like: Vec<&NoteSnapshot> = notes.iter().collect();
        let classified = self.classify_paths(&rule_like)?;
        let mut rules = Vec::new();
        let mut omitted = Vec::new();
        for (label, content) in classified {
            let parsed = wi_note::Note::parse(content.as_bytes())?;
            let lifecycle = parsed.lifecycle();
            let path = label.path.clone();
            if lifecycle.lifecycle_type != LifecycleType::Memory
                || !matches!(lifecycle.kind, Some(Kind::Rule | Kind::Preference))
            {
                omitted.push((path, "not-rule".into()));
                continue;
            }
            if label.eligibility != "current" {
                omitted.push((path, label.reason.unwrap_or_else(|| "plain".into())));
                continue;
            }
            rules.push(RuleNote {
                path,
                kind: if lifecycle.kind == Some(Kind::Rule) {
                    "rule".into()
                } else {
                    "preference".into()
                },
                summary: lifecycle.summary.clone(),
                content,
                valid_until: lifecycle
                    .applicability
                    .as_ref()
                    .and_then(|a| a.until.as_deref())
                    .into_iter()
                    .chain(label.freshness.get("citable_until").and_then(Value::as_str))
                    .filter_map(|t| wi_lifecycle::parse_time(t).ok())
                    .min()
                    .map(format_time),
                spans: label.spans,
            });
        }
        let earliest_valid_until = rules
            .iter()
            .filter_map(|r| r.valid_until.as_deref())
            .filter_map(|t| wi_lifecycle::parse_time(t).ok())
            .min()
            .map(format_time);
        Ok(RuleSet {
            rules,
            omitted,
            earliest_valid_until,
            generation,
        })
    }

    /// Procedure notes (`kind: procedure`) for skills.
    pub fn procedures(&self, mode: ProcedureMode) -> Result<Vec<Classified>> {
        let notes = self.lifecycle_notes_where(|n| {
            n.is_memory() && n.lifecycle.kind == Some(Kind::Procedure)
        })?;
        let procedures: Vec<&NoteSnapshot> = notes.iter().collect();
        let classified = self.classify_paths(&procedures)?;
        Ok(classified
            .into_iter()
            .map(|(label, _)| label)
            .filter(|label| mode == ProcedureMode::Direct || label.eligibility == "current")
            .collect())
    }

    /// A stable digest of the procedures a skills export delivers: the
    /// current, non-excluded procedure notes (discovery mode), each as its
    /// path and the hash of its bytes, in path order. Any process that may
    /// sync host skills compares it with the digest it last synced, so no
    /// process can consume another's change notice. It writes nothing.
    pub fn procedures_digest(&self) -> Result<String> {
        let mut entries: Vec<(String, String)> = Vec::new();
        for label in self.procedures(ProcedureMode::Discovery)? {
            if let Some(bytes) = crate::contained::read_rel(self.root(), &label.path)? {
                entries.push((label.path, wi_history::hash_bytes(&bytes)));
            }
        }
        entries.sort();
        let text =
            serde_json::to_string(&entries).map_err(|e| StoreError::Invalid(e.to_string()))?;
        Ok(wi_lifecycle::sha256(text.as_bytes()))
    }

    fn classify_paths(&self, notes: &[&NoteSnapshot]) -> Result<Vec<(Classified, String)>> {
        let read: Vec<(String, Vec<u8>)> = notes
            .iter()
            .filter_map(|n| {
                let bytes = crate::contained::read_rel(self.root(), &n.path).ok()??;
                Some((n.path.clone(), bytes))
            })
            .collect();
        let labels = self.classify(&read, &Value::Null)?;
        Ok(labels
            .into_iter()
            .zip(read)
            .map(|(label, (_, bytes))| (label, String::from_utf8_lossy(&bytes).into_owned()))
            .collect())
    }

    /// Whether `path` is under the archive directory.
    pub(crate) fn is_archived_path(&self, path: &str) -> bool {
        self.lifecycle_policy().is_archived_path(path)
    }

    /// Engine time as RFC 3339.
    pub(crate) fn now_text(&self) -> String {
        format_time(self.lifecycle_now())
    }

    /// Lint lifecycle notes (all memory notes, or one path). Reports only.
    pub fn lint(&self, path: Option<&str>) -> Result<Vec<LintReport>> {
        if !self.lifecycle_on() {
            return Ok(Vec::new());
        }
        let fresh: Vec<NoteSnapshot> = match path {
            Some(p) => {
                let (snap, _) = self
                    .note_snapshot(p)?
                    .ok_or_else(|| StoreError::NotFound { path: p.to_owned() })?;
                vec![snap]
            }
            None => Vec::new(),
        };
        let base = self.lifecycle_snapshot(&fresh)?;
        let targets: Vec<&NoteSnapshot> = match path {
            Some(p) => base.get(p).into_iter().collect(),
            None => base.notes().iter().filter(|n| n.is_memory()).collect(),
        };
        let evidence = self.evidence_for(targets.iter().copied());
        let paths: Vec<String> = targets.iter().map(|n| n.path.clone()).collect();
        let snapshot = base.with_evidence(evidence);
        let ctx = Context::new(&snapshot, self.lifecycle_now());
        let engine = self.lifecycle_engine();
        Ok(paths
            .iter()
            .filter_map(|p| snapshot.get(p))
            .map(|note| LintReport {
                path: note.path.clone(),
                findings: engine.lint(note, &ctx),
            })
            .filter(|r| path.is_some() || !r.findings.is_empty())
            .collect())
    }
}

/// Lint findings of one note.
#[derive(Debug, Clone, Serialize)]
pub struct LintReport {
    pub path: String,
    pub findings: Vec<wi_lifecycle::LintFinding>,
}

/// Keep current and plain hits; count withheld ones by reason.
fn filter_response(response: SearchResponse, labels: Vec<Classified>) -> CurrentSearch {
    let mut withheld: Vec<Withheld> = response.withheld.clone();
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut kept = Vec::new();
    for hit in response.hits.iter() {
        let path = hit.rel_path.to_string_lossy();
        match labels.iter().find(|l| l.path == path) {
            Some(label) if label.is_deliverable() => {
                hits.push(hit.clone());
                kept.push(label.clone());
            }
            Some(label) => {
                let reason = label.reason.clone().unwrap_or_else(|| "withheld".into());
                match withheld.iter_mut().find(|w| w.reason == reason) {
                    Some(w) => w.count += 1,
                    None => withheld.push(Withheld { reason, count: 1 }),
                }
            }
            None => {}
        }
    }
    CurrentSearch {
        response: SearchResponse {
            hits,
            withheld,
            ..response
        },
        labels: kept,
    }
}
