//! User-origin auto-completion, self-attested claims and per-gate work guidance.

mod common;

use common::*;
use serde_json::{Value, json};
use wi_lifecycle::*;

const USER_ORIGIN: &str = "origin: {kind: user, locator: file, captured_at: 2026-01-01T00:00:00Z}";
const AGENT_ORIGIN: &str =
    "origin: {kind: agent, locator: \"mcp:wi_write\", captured_at: 2026-01-01T00:00:00Z}";
/// Keys completion writes; `applicability` and `sources` are completed for
/// the gates only (absent, they read the same for a user-origin note).
const DEFAULT_KEYS: [&str; 5] = ["kind", "claim_kind", "summary", "entities", "scope"];

fn memory(path: &str, origin: &str, extra: &str, body: &str) -> (wi_note::Note, NoteSnapshot) {
    let text = format!("---\ntype: memory\ntitle: Coffee\n{origin}\n{extra}---\n{body}");
    let note = wi_note::Note::parse(text.as_bytes()).unwrap();
    let snap = NoteSnapshot::from_note(path, &note);
    assert!(snap.unparseable.is_none(), "{:?}", snap.unparseable);
    (note, snap)
}

fn evaluate(snap: &NoteSnapshot) -> AdoptionOutcome {
    let snapshot = Snapshot::new(vec![snap.clone()], Evidence::default());
    Engine::default().evaluate_adoption(snap, &Context::new(&snapshot, day(1)))
}

fn first_edit(outcome: &AdoptionOutcome) -> &NoteEdit {
    match outcome {
        AdoptionOutcome::Active { edits, .. }
        | AdoptionOutcome::Candidate { edits, .. }
        | AdoptionOutcome::Deferred { edits, .. } => &edits[0],
        other => panic!("expected edits, got {other:?}"),
    }
}

fn gates_of(outcome: &AdoptionOutcome) -> &GateReport {
    match outcome {
        AdoptionOutcome::Active { gates, .. }
        | AdoptionOutcome::Candidate { gates, .. }
        | AdoptionOutcome::Deferred { gates, .. } => gates,
        other => panic!("expected gates, got {other:?}"),
    }
}

fn lint_codes(snap: &NoteSnapshot) -> Vec<LintCode> {
    let snapshot = Snapshot::new(vec![snap.clone()], Evidence::default());
    Engine::default()
        .lint(snap, &Context::new(&snapshot, day(1)))
        .into_iter()
        .map(|f| f.code)
        .collect()
}

#[test]
fn user_memory_with_only_title_and_body_becomes_active_with_all_defaults() {
    let (note, snap) = memory(
        "notes/coffee.md",
        USER_ORIGIN,
        "",
        "# I drink oat milk\n\nmore",
    );
    let outcome = evaluate(&snap);
    assert!(
        matches!(outcome, AdoptionOutcome::Active { .. }),
        "{outcome:?}"
    );
    let edit = first_edit(&outcome);
    assert_eq!(edit.set["status"], json!("active"));
    assert_eq!(edit.set["kind"], json!("fact"));
    assert_eq!(edit.set["claim_kind"], json!("historical"));
    assert_eq!(edit.set["summary"], json!("I drink oat milk"));
    assert_eq!(edit.set["entities"], json!([]));
    assert_eq!(edit.set["scope"], json!(DEFAULT_USER_SCOPE));
    assert!(!edit.set.contains_key("applicability"), "{edit:?}");
    assert!(!edit.set.contains_key("sources"), "{edit:?}");
    assert_eq!(edit.set["adoption"]["completed"], json!(DEFAULT_KEYS));
    assert!(
        edit.set["adoption"].get("gates").is_none(),
        "gates are written only when one failed: {edit:?}"
    );

    // The written note parses back as a complete, still-user claim.
    let written = edit.apply(&note).unwrap();
    let reread = NoteSnapshot::from_note("notes/coffee.md", &written);
    let (_, refill) = complete_user_defaults(&reread);
    assert!(refill.is_empty(), "{refill:?}");
    assert_eq!(reread.lifecycle.status, Some(wi_note::Status::Active));
    assert_eq!(
        reread.lifecycle.origin.map(|o| o.kind),
        Some(wi_note::OriginKind::User)
    );
}

#[test]
fn null_fields_are_filled_like_absent_ones() {
    let (_, snap) = memory(
        "n.md",
        USER_ORIGIN,
        "kind: null\nsummary: null\nscope: ~\n",
        "body line",
    );
    let (_, filled) = complete_user_defaults(&snap);
    let keys: Vec<&str> = filled.iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, DEFAULT_KEYS);
}

#[test]
fn present_values_are_never_overwritten() {
    let extra = "kind: decision\nclaim_kind: intent\nsummary: Mine\nscope: team\n\
                 entities: [{id: \"svc:x\", label: X, source: null}]\n";
    let (_, snap) = memory("n.md", USER_ORIGIN, extra, "Body text");
    let outcome = evaluate(&snap);
    let edit = first_edit(&outcome);
    for key in ["kind", "claim_kind", "summary", "scope", "entities"] {
        assert!(!edit.set.contains_key(key), "{key} overwritten: {edit:?}");
    }
    assert!(!edit.set.contains_key("applicability"), "{edit:?}");
    assert!(!edit.set.contains_key("sources"), "{edit:?}");
    assert!(edit.set["adoption"].get("completed").is_none(), "{edit:?}");
    assert!(matches!(outcome, AdoptionOutcome::Active { .. }));
}

#[test]
fn complete_user_note_gets_no_completion_record() {
    let snap = Claim::fact("a.md")
        .set("claim_kind", json!("historical"))
        .snap();
    let outcome = evaluate(&snap);
    let edit = first_edit(&outcome);
    assert!(edit.set["adoption"].get("completed").is_none(), "{edit:?}");
}

#[test]
fn agent_origin_note_is_not_completed_and_fails_classification() {
    let (_, snap) = memory("n.md", AGENT_ORIGIN, "", "# I drink oat milk");
    let (completed, filled) = complete_user_defaults(&snap);
    assert!(filled.is_empty());
    assert_eq!(completed, snap);
    let outcome = evaluate(&snap);
    assert!(
        matches!(outcome, AdoptionOutcome::Candidate { .. }),
        "{outcome:?}"
    );
    assert!(!gates_of(&outcome).results["classification"].passed());
    let edit = first_edit(&outcome);
    assert!(!edit.set.contains_key("kind"));
    assert!(edit.set["adoption"].get("completed").is_none());
}

#[test]
fn user_fact_without_pins_passes_sources_and_lint() {
    let user = Claim::fact("u.md").sources(Vec::new()).snap();
    let outcome = evaluate(&user);
    assert!(
        gates_of(&outcome).results["sources"].passed(),
        "{outcome:?}"
    );
    assert!(!lint_codes(&user).contains(&LintCode::UnsupportedClaim));
    let decision = Claim::fact("d.md").set("kind", json!("decision")).snap();
    assert!(gates_of(&evaluate(&decision)).results["sources"].passed());

    let agent = Claim::fact("a.md").sources(Vec::new()).agent().snap();
    let outcome = evaluate(&agent);
    assert_eq!(
        gates_of(&outcome).results["sources"],
        GateResult::Fail("no source pin".into())
    );
    assert!(lint_codes(&agent).contains(&LintCode::UnsupportedClaim));
}

#[test]
fn missing_observed_at_lint_stays_for_user_current_state() {
    let user = Claim::fact("u.md").remove("observed_at").snap();
    assert!(lint_codes(&user).contains(&LintCode::MissingObservedAt));
}

#[test]
fn broken_pin_on_user_note_still_fails() {
    let user = Claim::fact("u.md")
        .sources(vec![pin("src.md", b"original", None)])
        .snap();
    let evidence = Evidence::default().with("src.md", SourceState::present(&b"changed"[..], None));
    let snapshot = Snapshot::new(vec![user.clone()], evidence);
    let ctx = Context::new(&snapshot, day(1));
    let outcome = Engine::default().evaluate_adoption(&user, &ctx);
    assert_eq!(
        gates_of(&outcome).results["sources"],
        GateResult::Fail("hash mismatch src.md".into())
    );
    let codes: Vec<LintCode> = Engine::default()
        .lint(&user, &ctx)
        .into_iter()
        .map(|f| f.code)
        .collect();
    assert!(codes.contains(&LintCode::BrokenRequiredSource));
}

#[test]
fn guidance_names_missing_fields_per_gate() {
    let (_, snap) = memory("n.md", AGENT_ORIGIN, "", "body");
    let outcome = evaluate(&snap);
    let AdoptionOutcome::Candidate { work, .. } = &outcome else {
        panic!("expected candidate, got {outcome:?}");
    };
    let action = &work[0].next_action;
    assert!(
        action.starts_with(
            "classification: add kind, claim_kind, summary, applicability, entities, scope"
        ),
        "{action}"
    );
    assert!(
        action.contains("sources: add a `sources` list of pins."),
        "{action}"
    );
    assert!(!action.contains("rerun the gates"), "{action}");
}

#[test]
fn guidance_covers_each_gate() {
    let gates = GateReport {
        results: [
            ("conflict", "conflicts with a.md, b.md"),
            ("lint", "expired-applicability"),
            ("origin", "origin.kind is not user"),
            ("supersedes", "invalid target old.md unresolved"),
            ("literal", "attachment-unavailable: src.md: missing"),
            ("sources", "missing src.md"),
        ]
        .into_iter()
        .map(|(g, r)| (g.to_string(), GateResult::Fail(r.into())))
        .chain([("classification".to_string(), GateResult::Pass)])
        .collect(),
        literal: false,
    };
    let text = gate_guidance(&gates);
    for needle in [
        "conflict: decide against a.md, b.md",
        "lint: fix the reported codes (expired-applicability)",
        "origin: only the user can adopt",
        "supersedes: fix the supersedes list or supply newer evidence (old.md unresolved)",
        "literal: make the pinned source spans deliverable",
        "sources: fix or re-pin the failing pins (missing src.md)",
    ] {
        assert!(text.contains(needle), "{needle} not in {text}");
    }
    assert!(!text.contains("classification"), "{text}");
}

#[test]
fn summary_extraction() {
    assert_eq!(
        summary_from_body("\n\n## Heading text \n", None, "a.md"),
        "Heading text"
    );
    assert_eq!(
        summary_from_body("- item one\n- two", None, "a.md"),
        "item one"
    );
    assert_eq!(summary_from_body("> * quoted\n", None, "a.md"), "quoted");
    assert_eq!(summary_from_body("12. step\n", None, "a.md"), "step");
    assert_eq!(summary_from_body("#\n---\nreal\n", None, "a.md"), "real");
    assert_eq!(summary_from_body("  \n", Some("Title"), "a.md"), "Title");
    assert_eq!(summary_from_body("", None, "dir/stem.md"), "stem");
    let long: String = "가나다".repeat(100);
    let summary = summary_from_body(&long, None, "a.md");
    assert_eq!(summary.chars().count(), 200);
    assert!(long.starts_with(&summary));

    let (_, snap) = memory("dir/empty.md", USER_ORIGIN, "", "");
    assert_eq!(snap.default_summary.as_deref(), Some("Coffee"));
}

#[test]
fn applicability_without_conditions_passes_for_user_and_is_untouched() {
    let extra = "applicability: {from: null}\n";
    let (note, snap) = memory("n.md", USER_ORIGIN, extra, "body");
    let outcome = evaluate(&snap);
    assert!(
        gates_of(&outcome).results["classification"].passed(),
        "{outcome:?}"
    );
    let edit = first_edit(&outcome);
    assert!(!edit.set.contains_key("applicability"), "{edit:?}");
    let written = edit.apply(&note).unwrap();
    let text = String::from_utf8(written.to_bytes().to_vec()).unwrap();
    assert!(text.contains("\napplicability: {from: null}\n"), "{text}");

    // Adopted, the note is current: eligibility applies the same reading.
    let active = NoteSnapshot::from_note("n.md", &written);
    let snapshot = Snapshot::new(vec![active.clone()], Evidence::default());
    let eligibility = Engine::default().eligibility(&active, &Context::new(&snapshot, day(1)));
    assert!(eligibility.is_current(), "{eligibility:?}");

    // An agent note keeps the strict rule.
    let (_, agent) = memory(
        "a.md",
        AGENT_ORIGIN,
        "kind: event\nclaim_kind: historical\nsummary: s\nentities: []\nscope: x\n\
         sources: []\napplicability: {from: null}\n",
        "body",
    );
    assert_eq!(
        gates_of(&evaluate(&agent)).results["classification"],
        GateResult::Fail("null applicability.conditions".into())
    );
}

#[test]
fn snapshot_with_default_summary_round_trips_through_the_index() {
    let (_, snap) = memory("n.md", USER_ORIGIN, "", "line");
    let text = snap.materialize().expect("materializes");
    assert_eq!(NoteSnapshot::from_materialized("n.md", &text), Some(snap));
    let complete = Claim::fact("c.md").snap();
    assert!(complete.default_summary.is_none());
    let text: Value = serde_json::from_str(&complete.materialize().unwrap()).unwrap();
    assert!(text.get("default_summary").is_none());
}

const USER_CLASSIFIED: &str = "kind: event\nclaim_kind: historical\nsummary: s\nentities: []\n\
                               scope: x\nsources: []\n";

#[test]
fn explicit_null_conditions_on_a_user_note_stay_unresolved() {
    let extra = format!("{USER_CLASSIFIED}applicability: {{from: null, conditions: null}}\n");
    let (_, snap) = memory("n.md", USER_ORIGIN, &extra, "body");
    assert!(!snap.conditions_absent);
    assert_eq!(
        gates_of(&evaluate(&snap)).results["classification"],
        GateResult::Fail("null applicability.conditions".into())
    );
    let (_, active) = memory(
        "n.md",
        USER_ORIGIN,
        &format!("status: active\n{extra}"),
        "body",
    );
    let snapshot = Snapshot::new(vec![active.clone()], Evidence::default());
    let eligibility = Engine::default().eligibility(&active, &Context::new(&snapshot, day(1)));
    assert!(
        matches!(
            eligibility,
            Eligibility::Withheld(Withheld::UnresolvedConditions)
        ),
        "{eligibility:?}"
    );
}

#[test]
fn absent_conditions_are_recorded_on_the_snapshot() {
    let extra = format!("{USER_CLASSIFIED}applicability: {{from: null}}\n");
    let (_, mapping) = memory("a.md", USER_ORIGIN, &extra, "body");
    assert!(mapping.conditions_absent);
    let (_, none) = memory("b.md", USER_ORIGIN, "", "body");
    assert!(none.conditions_absent);
    let text = mapping.materialize().expect("materializes");
    assert_eq!(
        NoteSnapshot::from_materialized("a.md", &text),
        Some(mapping)
    );
}

#[test]
fn summary_markers_need_following_whitespace() {
    for kept in [
        "-5 degrees is the cutoff",
        "3.14 is the ratio",
        "2026. year",
        "+1 vote",
        "#tag first",
        "1)x",
    ] {
        assert_eq!(summary_from_body(kept, None, "t.md"), kept, "{kept}");
    }
    for (line, text) in [
        ("- item", "item"),
        ("1. item", "item"),
        ("2) item", "item"),
        ("# Title", "Title"),
        ("###### Deep", "Deep"),
        ("> - quoted item", "quoted item"),
        ("## 1. step", "step"),
    ] {
        assert_eq!(summary_from_body(line, None, "t.md"), text, "{line}");
    }
}
