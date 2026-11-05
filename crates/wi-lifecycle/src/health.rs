//! Procedure health: model-free checks that turn a procedure note which
//! agents cannot use well into one `procedure` work item. The item key
//! depends only on the procedure path and its duplicate partners, so a
//! repeated pass refreshes the same item, and the problem codes are the
//! observed condition, so a closed item reopens only when they change.

use std::collections::BTreeMap;

use serde::Serialize;
use wi_note::{Kind, Status};

use crate::eligibility::AttachedSpan;
use crate::freshness::{FreshnessState, freshness};
use crate::lint::lint;
use crate::model::{Context, NoteSnapshot, Policy};
use crate::work::{Trigger, WorkItem, WorkKind};

/// Bytes an MCP tool result may carry. A procedure whose delivered form
/// ([`delivery_size`]) is larger than this cannot reach an agent in one piece.
pub const MCP_MAX_BYTES: u64 = 16_384;

/// Room an MCP result keeps for its envelope, the omission counter it
/// reserves, and the fixed fields of one hit (label, hashes, permalink,
/// freshness, tier marker).
pub const DELIVERY_RESERVE: u64 = 1_024;

/// Conservative size, in bytes, of one note delivered as an MCP hit: the
/// whole file as a JSON string, its frontmatter again as JSON metadata and
/// copied fields (sources, title, status), the path several times, and
/// [`DELIVERY_RESERVE`]. Never smaller than the serialized hit, so a note
/// this function accepts below [`MCP_MAX_BYTES`] fits.
pub fn delivery_size(path: &str, bytes: &[u8]) -> u64 {
    let text = String::from_utf8_lossy(bytes);
    let front = wi_note::Note::parse(bytes)
        .ok()
        .and_then(|note| note.frontmatter().map(|f| f.byte_range.clone()))
        .and_then(|range| text.get(range).map(json_len))
        .unwrap_or(0);
    json_len(&text) + 2 * front + 4 * json_len(path) + DELIVERY_RESERVE
}

/// [`delivery_size`] plus the resolved source spans a delivery attaches
/// to the hit (`sources: {pins, spans}`), which a lifecycle hit cannot
/// shrink. Never smaller than the serialized hit with those spans.
pub fn delivery_size_with_spans(path: &str, bytes: &[u8], spans: &[AttachedSpan]) -> u64 {
    if spans.is_empty() {
        return delivery_size(path, bytes);
    }
    // The `{"pins": .., "spans": ..}` wrapper around the copied sources.
    const WRAPPER: u64 = 32;
    let spans = serde_json::to_vec(spans).map_or(u64::MAX / 8, |s| s.len() as u64);
    delivery_size(path, bytes) + spans + WRAPPER
}

/// Length of `text` as a JSON string literal.
fn json_len(text: &str) -> u64 {
    serde_json::to_string(text).map_or(u64::MAX / 8, |s| s.len() as u64)
}

/// One problem of one procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcedureProblem {
    /// Lint reports a finding that is not advisory.
    Lint,
    /// A pinned source drifted, went missing or aged out.
    StaleSource,
    /// Another live procedure carries the same `skill_id`.
    DuplicateSkillId,
    /// The delivered note would be larger than [`MCP_MAX_BYTES`].
    TooLarge,
    /// The procedure is excluded and no live procedure replaces it.
    ExcludedWithoutReplacement,
}

impl ProcedureProblem {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Lint => "lint",
            Self::StaleSource => "stale-source",
            Self::DuplicateSkillId => "duplicate-skill-id",
            Self::TooLarge => "too-large",
            Self::ExcludedWithoutReplacement => "excluded-without-replacement",
        }
    }

    fn action(&self) -> &'static str {
        match self {
            Self::Lint => "fix the lint findings (wi_lint) in a new candidate revision",
            Self::StaleSource => "re-read the pinned sources and renew or revise the procedure",
            Self::DuplicateSkillId => "merge the procedures or give one a new skill_id",
            Self::TooLarge => "split the procedure into smaller procedures",
            Self::ExcludedWithoutReplacement => "write a replacement procedure or archive it",
        }
    }
}

/// A procedure whose status still competes (not superseded, rejected or
/// archived) outside the archive directory.
fn live_procedure(note: &NoteSnapshot, policy: &Policy) -> bool {
    note.is_memory()
        && note.lifecycle.kind == Some(Kind::Procedure)
        && !matches!(
            note.lifecycle.status,
            Some(Status::Superseded | Status::Rejected | Status::Archived)
        )
        && !policy.is_archived_path(&note.path)
}

fn skill_id(note: &NoteSnapshot) -> Option<&str> {
    note.lifecycle
        .skill_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

/// Problems of every live procedure, keyed by path. `sizes` holds the
/// delivery size ([`delivery_size`]) of each note path the host knows (a
/// missing size is not checked).
pub fn procedure_problems(
    ctx: &Context,
    policy: &Policy,
    sizes: &BTreeMap<String, u64>,
) -> BTreeMap<String, (Vec<ProcedureProblem>, Vec<String>)> {
    let mut out = BTreeMap::new();
    if !policy.lifecycle_on {
        return out;
    }
    let procedures: Vec<&NoteSnapshot> = ctx
        .snapshot
        .notes()
        .iter()
        .filter(|n| live_procedure(n, policy))
        .collect();
    let mut by_id: BTreeMap<&str, Vec<&NoteSnapshot>> = BTreeMap::new();
    for note in &procedures {
        if let Some(id) = skill_id(note) {
            by_id.entry(id).or_default().push(note);
        }
    }
    for note in &procedures {
        let mut problems = Vec::new();
        let mut related = Vec::new();
        if lint(note, ctx, policy)
            .iter()
            .any(|f| !f.code.informational())
        {
            problems.push(ProcedureProblem::Lint);
        }
        let fresh = freshness(&note.lifecycle, &ctx.snapshot.evidence, policy, ctx.now);
        if matches!(
            fresh.state,
            FreshnessState::Drifted(_)
                | FreshnessState::SourceMissing(_)
                | FreshnessState::NoSnapshot(_)
                | FreshnessState::Stale
                | FreshnessState::TtlExpired
        ) {
            problems.push(ProcedureProblem::StaleSource);
        }
        let excluded = note.lifecycle.excluded == Some(true);
        if let Some(id) = skill_id(note) {
            let peers = &by_id[id];
            // Excluded procedures do not compete for their id.
            let competing: Vec<&&NoteSnapshot> = peers
                .iter()
                .filter(|p| p.lifecycle.excluded != Some(true))
                .collect();
            if !excluded && competing.len() > 1 {
                problems.push(ProcedureProblem::DuplicateSkillId);
                related.extend(
                    competing
                        .iter()
                        .filter(|p| p.path != note.path)
                        .map(|p| p.path.clone()),
                );
            }
            if excluded && competing.is_empty() {
                problems.push(ProcedureProblem::ExcludedWithoutReplacement);
            }
        } else if excluded {
            problems.push(ProcedureProblem::ExcludedWithoutReplacement);
        }
        if sizes.get(&note.path).is_some_and(|n| *n > MCP_MAX_BYTES) {
            problems.push(ProcedureProblem::TooLarge);
        }
        if !problems.is_empty() {
            related.sort();
            out.insert(note.path.clone(), (problems, related));
        }
    }
    out
}

/// One `procedure` work item per procedure with problems.
pub fn procedure_health(
    ctx: &Context,
    policy: &Policy,
    sizes: &BTreeMap<String, u64>,
) -> Vec<WorkItem> {
    procedure_problems(ctx, policy, sizes)
        .into_iter()
        .map(|(path, (problems, related))| {
            let codes: Vec<&str> = problems.iter().map(ProcedureProblem::code).collect();
            let actions: Vec<&str> = problems.iter().map(ProcedureProblem::action).collect();
            WorkItem::new(
                WorkKind::Procedure,
                &path,
                related,
                format!("procedure needs upkeep: {}", codes.join(", ")),
                actions.join("; "),
                Trigger::Session,
                ctx.now,
            )
            .with_condition(format!("procedure: {}", codes.join(",")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Evidence, Snapshot};

    fn procedure(path: &str, extra: &str) -> NoteSnapshot {
        let text = format!(
            "---\ntype: memory\nkind: procedure\nstatus: active\nclaim_kind: intent\nsummary: Do it\nentities: []\nscope: work\norigin:\n  kind: user\n{extra}---\nDo it.\n"
        );
        NoteSnapshot::from_bytes(path, text.as_bytes())
    }

    fn problems(
        notes: Vec<NoteSnapshot>,
        sizes: &[(&str, u64)],
    ) -> BTreeMap<String, Vec<&'static str>> {
        let snapshot = Snapshot::new(notes, Evidence::default());
        let ctx = Context::new(
            &snapshot,
            crate::time::parse_time("2026-11-07T00:00:00Z").unwrap(),
        );
        let sizes = sizes.iter().map(|(p, n)| (p.to_string(), *n)).collect();
        procedure_problems(&ctx, &Policy::default(), &sizes)
            .into_iter()
            .map(|(path, (list, _))| (path, list.iter().map(ProcedureProblem::code).collect()))
            .collect()
    }

    #[test]
    fn size_duplicates_and_exclusion_are_reported_with_stable_keys() {
        let notes = vec![
            procedure("a.md", "skill_id: deploy\n"),
            procedure("b.md", "skill_id: deploy\n"),
            procedure("c.md", "skill_id: solo\nexcluded: true\n"),
            procedure("d.md", "skill_id: big\n"),
            procedure("archive/e.md", "skill_id: deploy\n"),
        ];
        let found = problems(notes.clone(), &[("d.md", MCP_MAX_BYTES + 1), ("a.md", 10)]);
        assert_eq!(found["a.md"], ["duplicate-skill-id"]);
        assert_eq!(found["b.md"], ["duplicate-skill-id"]);
        assert_eq!(found["c.md"], ["excluded-without-replacement"]);
        assert_eq!(found["d.md"], ["too-large"]);
        assert!(!found.contains_key("archive/e.md"));

        let snapshot = Snapshot::new(notes, Evidence::default());
        let ids = |now| {
            procedure_health(
                &Context::new(&snapshot, now),
                &Policy::default(),
                &BTreeMap::new(),
            )
            .into_iter()
            .map(|i| i.id)
            .collect::<Vec<_>>()
        };
        let now = crate::time::parse_time("2026-11-07T00:00:00Z").unwrap();
        assert_eq!(
            ids(now),
            ids(now + chrono::TimeDelta::days(1)),
            "keys do not depend on time"
        );
    }

    #[test]
    fn a_replacement_settles_an_excluded_procedure() {
        let found = problems(
            vec![
                procedure("old.md", "skill_id: deploy\nexcluded: true\n"),
                procedure("new.md", "skill_id: deploy\n"),
            ],
            &[],
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn delivery_size_counts_json_escaping_and_metadata() {
        let plain = "---\nsummary: x\n---\nabc\n";
        let small = delivery_size("a.md", plain.as_bytes());
        assert!(small >= plain.len() as u64 + DELIVERY_RESERVE);
        let quoted = format!("---\nsummary: x\n---\n{}\n", "\\\"".repeat(6000));
        assert!((quoted.len() as u64) < MCP_MAX_BYTES);
        assert!(delivery_size("a.md", quoted.as_bytes()) > MCP_MAX_BYTES);
    }
}
