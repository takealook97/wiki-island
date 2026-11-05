//! Regressions for evidence delivery, freshness, validation and work reopening.

mod common;

use std::collections::BTreeSet;

use common::*;
use serde_json::json;
use wi_lifecycle::*;

const R: &str = "source.md";
const S: &[u8] = b"X (beta, EU only)";

fn supported(path: &str) -> Claim {
    Claim::fact(path).sources(vec![pin(R, S, Some((0, S.len())))])
}

fn ev() -> Evidence {
    Evidence::default().with(R, SourceState::present(S, Some(day(0))))
}

fn literal_gate(outcome: &AdoptionOutcome) -> GateResult {
    match outcome {
        AdoptionOutcome::Candidate { gates, .. } => gates.results["literal"].clone(),
        other => panic!("expected candidate, got {other:?}"),
    }
}

/// Mark `claim` active with a non-literal adoption record.
fn adopted_paraphrase(claim: Claim) -> Claim {
    claim.active().set(
        "adoption",
        json!({"at": null, "by": null, "gates": {}, "literal": false}),
    )
}

/// A pin without a locator supports the whole source by its hash: a
/// paraphrase citing whole files adopts and is delivered without spans.
#[test]
fn paraphrase_citing_a_whole_file_adopts_without_attachments() {
    let c = Claim::fact("a.md")
        .sources(vec![pin(R, S, None)])
        .set("summary", json!("X"));
    let s = Snapshot::new(vec![c.snap()], ev());
    let outcome = Engine::default().evaluate_adoption(&c.snap(), &Context::new(&s, day(1)));
    assert!(
        matches!(&outcome, AdoptionOutcome::Active { gates, .. } if !gates.literal),
        "{outcome:?}"
    );
    let active = adopted_paraphrase(c);
    let s = Snapshot::new(vec![active.snap()], ev());
    let ctx = Context::new(&s, day(1));
    let e = Engine::default();
    assert_eq!(e.eligibility(&active.snap(), &ctx), Eligibility::Current);
    assert!(e.attached_spans(&active.snap(), &ctx).is_empty());
}

/// A whole-file pin never excuses another pin's span that cannot be delivered.
#[test]
fn a_whole_file_pin_does_not_cover_a_broken_span() {
    let mut broken = pin(R, S, Some((0, S.len())));
    broken["span_hash"] = json!("0".repeat(64));
    let c = adopted_paraphrase(
        Claim::fact("a.md")
            .sources(vec![pin(R, S, None), broken])
            .set("summary", json!("X")),
    );
    let s = Snapshot::new(vec![c.snap()], ev());
    let ctx = Context::new(&s, day(1));
    assert!(matches!(
        Engine::default().eligibility(&c.snap(), &ctx),
        Eligibility::Withheld(Withheld::AttachmentUnavailable(_))
    ));
}

#[test]
fn mid_codepoint_span_fails_sources_literal_and_lint() {
    let b = "한글 (beta, EU only)".as_bytes();
    let c = Claim::fact("a.md").sources(vec![pin(R, b, Some((1, 2)))]);
    let evidence = Evidence::default().with(R, SourceState::present(b, Some(day(0))));
    let s = Snapshot::new(vec![c.snap()], evidence);
    let ctx = Context::new(&s, day(1));
    let e = Engine::default();
    let outcome = e.evaluate_adoption(&c.snap(), &ctx);
    let AdoptionOutcome::Candidate { gates, .. } = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(!gates.results["sources"].passed());
    assert!(!gates.results["literal"].passed());
    assert!(
        e.lint(&c.snap(), &ctx)
            .iter()
            .any(|f| f.code == LintCode::UnsupportedMarkup && f.message.contains("UTF-8")),
    );
    // Forged as active, it is withheld rather than delivered without its span.
    let forged = adopted_paraphrase(c);
    assert!(matches!(
        e.eligibility(&forged.snap(), &ctx),
        Eligibility::Withheld(Withheld::AttachmentUnavailable(_))
    ));
}

#[test]
fn page_locator_is_not_a_deliverable_attachment() {
    let mut page = pin(R, S, None);
    page["locator"] = json!({"page": {"n": 1, "label": null}});
    let c = Claim::fact("a.md").sources(vec![page]);
    let s = Snapshot::new(vec![c.snap()], ev());
    let outcome = Engine::default().evaluate_adoption(&c.snap(), &Context::new(&s, day(1)));
    assert!(!literal_gate(&outcome).passed());
}

#[test]
fn controls_literal_and_deliverable_paraphrase_adopt() {
    let e = Engine::default();
    let k = "한글".as_bytes();
    let evidence = Evidence::default().with(R, SourceState::present(k, Some(day(0))));
    let lit = Claim::fact("literal.md")
        .sources(vec![pin(R, k, Some((0, 6)))])
        .set("summary", json!("한글"));
    let s = Snapshot::new(vec![lit.snap()], evidence);
    assert!(matches!(
        e.evaluate_adoption(&lit.snap(), &Context::new(&s, day(1))),
        AdoptionOutcome::Active { gates, .. } if gates.literal
    ));
    let para = supported("p.md").set("summary", json!("X"));
    let s = Snapshot::new(vec![para.snap()], ev());
    let ctx = Context::new(&s, day(1));
    let AdoptionOutcome::Active { edits, gates, .. } = e.evaluate_adoption(&para.snap(), &ctx)
    else {
        panic!()
    };
    assert!(!gates.literal);
    let n = NoteSnapshot::from_note("p.md", &edits[0].apply(&para.note()).unwrap());
    let s = s.with_note(n.clone());
    let ctx = Context::new(&s, day(1));
    assert_eq!(e.eligibility(&n, &ctx), Eligibility::Current);
    let spans = e.attached_spans(&n, &ctx);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].text, "X (beta, EU only)");
}

#[test]
fn missing_evidence_time_cannot_outrank_or_supersede() {
    let t = supported("target.md").active().observed(day(10));
    let c = supported("candidate.md")
        .agent()
        .observed(day(20))
        .supersedes(&["target.md"]);
    let evidence = Evidence::default().with(R, SourceState::present(S, None));
    let s = Snapshot::new(vec![t.snap(), c.snap()], evidence);
    let e = Engine::default();
    let ctx = Context::new(&s, day(20));
    assert_eq!(
        e.effective_observed_at(&c.snap(), &s.evidence).unwrap(),
        None
    );
    assert_eq!(
        e.validate_supersedes(&c.snap(), &t.snap(), &ctx),
        Err(InvalidTarget::UnknownEvidenceTime)
    );
    let outcome = e.evaluate_adoption(&c.snap(), &ctx);
    let AdoptionOutcome::Candidate { gates, work, edits } = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(!gates.results["sources"].passed());
    assert!(!gates.results["supersedes"].passed());
    assert!(edits.iter().all(|edit| edit.path == "candidate.md"));
    assert!(work.iter().any(|w| w.kind == WorkKind::Gate));
    assert!(e.eligibility(&t.snap(), &ctx).is_current());
    // A forged active agent claim with an unknown evidence time is not citable.
    let forged = supported("forged.md").agent().active();
    let s = Snapshot::new(vec![forged.snap()], s.evidence.clone());
    assert!(matches!(
        e.eligibility(&forged.snap(), &Context::new(&s, day(1))),
        Eligibility::Withheld(Withheld::Freshness(FreshnessState::MissingEvidenceTime(_)))
    ));
}

#[test]
fn one_unknown_pin_among_several_is_not_omitted_from_the_minimum() {
    const OTHER_REFERENCE: &str = "other.md";
    let c = Claim::fact("a.md").agent().observed(day(20)).sources(vec![
        pin(R, S, Some((0, S.len()))),
        pin(OTHER_REFERENCE, S, Some((0, S.len()))),
    ]);
    let evidence = Evidence::default()
        .with(R, SourceState::present(S, Some(day(15))))
        .with(OTHER_REFERENCE, SourceState::present(S, None));
    let e = Engine::default();
    assert_eq!(e.effective_observed_at(&c.snap(), &evidence).unwrap(), None);
    let known = evidence.with(OTHER_REFERENCE, SourceState::present(S, Some(day(3))));
    assert_eq!(
        e.effective_observed_at(&c.snap(), &known).unwrap(),
        Some(day(3))
    );
    // User claims keep their own time without engine evidence times.
    let user = supported("u.md").observed(day(20));
    let none = Evidence::default().with(R, SourceState::present(S, None));
    assert_eq!(
        e.effective_observed_at(&user.snap(), &none).unwrap(),
        Some(day(20))
    );
}

#[test]
fn null_conditions_fail_classification_and_withhold_reads() {
    // Agent origin: a user note reads absent conditions as `[]`.
    let c = supported("a.md")
        .applicability("conditions", json!(null))
        .agent();
    let s = Snapshot::new(vec![c.snap()], ev());
    let e = Engine::default();
    let outcome = e.evaluate_adoption(&c.snap(), &Context::new(&s, day(1)));
    let AdoptionOutcome::Candidate { gates, .. } = &outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        gates.results["classification"],
        GateResult::Fail("null applicability.conditions".into())
    );
    let active = c.clone().active();
    let s = Snapshot::new(vec![active.snap()], ev());
    assert_eq!(
        e.eligibility(&active.snap(), &Context::new(&s, day(1))),
        Eligibility::Withheld(Withheld::UnresolvedConditions)
    );
    // Null bounds stay unbounded.
    let bounds = supported("b.md")
        .applicability("from", json!(null))
        .applicability("until", json!(null));
    let s = Snapshot::new(vec![bounds.snap()], ev());
    assert!(matches!(
        e.evaluate_adoption(&bounds.snap(), &Context::new(&s, day(1))),
        AdoptionOutcome::Active { .. }
    ));
}

fn entry(id: &str, d: i64, newer: bool) -> DeferredEntry {
    DeferredEntry {
        id: id.into(),
        action: DeferredAction::Loss,
        requests: vec![LossRequest {
            target: id.into(),
            entities: vec![id.into()],
            kind: LossKind::Archive,
        }],
        actor: Actor::Agent,
        enqueued_at: day(d),
        newer_evidence: newer,
        target_hash: None,
    }
}

#[test]
fn queue_dedups_non_adjacent_ids_with_an_explicit_merge() {
    let q = DeferredQueue::new(vec![
        entry("a", 0, false),
        entry("b", 1, false),
        entry("a", 2, true),
    ]);
    let ids: Vec<&str> = q.entries().iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b"]);
    let a = &q.entries()[0];
    assert_eq!(a.enqueued_at, day(0));
    assert!(a.newer_evidence);
    let counts = ActiveCounts {
        per_entity: [("a".into(), 100), ("b".into(), 100)].into(),
        total: 1000,
    };
    let r = q.resume(&BudgetLedger::default(), &counts, day(3));
    assert_eq!(r.ready.len(), 2);
    // A retried push refreshes the entry and keeps its FIFO position.
    let pushed = DeferredQueue::new(vec![entry("a", 0, false), entry("b", 1, false)])
        .push(entry("a", 5, false));
    let ids: Vec<&str> = pushed.entries().iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b"]);
    assert_eq!(pushed.entries()[0].enqueued_at, day(0));
}

#[test]
fn malformed_durations_are_refused_and_linted() {
    for bad in ["P1D1D", "PT1S1H", "P1M1Y", "PT1M1H", "P1D1Y"] {
        assert!(IsoDuration::parse(bad).is_err(), "{bad}");
    }
    assert_eq!(IsoDuration::parse("P1W1D").unwrap(), IsoDuration::days(8));
    let c = supported("a.md")
        .active()
        .set("check", json!({"ttl": "P1D1D"}));
    let s = Snapshot::new(vec![c.snap()], ev());
    let ctx = Context::new(&s, day(1));
    assert!(
        Engine::default()
            .lint(&c.snap(), &ctx)
            .iter()
            .any(|f| f.code == LintCode::UnsupportedMarkup && f.message.contains("P1D1D"))
    );
    assert!(!Engine::default().eligibility(&c.snap(), &ctx).is_current());
}

#[test]
fn short_credential_assignments_are_dropped_and_placeholders_kept() {
    let none = BTreeSet::new();
    let e = Engine::default();
    for text in [
        "password: abc",
        "password: hunter2",
        "api_key: abc123",
        "token = t0k",
    ] {
        assert!(
            matches!(e.capture_filter(text, &none), FilterOutcome::Drop(_)),
            "{text}"
        );
    }
    for text in [
        "password = <redacted>",
        "api_key: ${API_KEY}",
        "secret: ****",
    ] {
        assert_eq!(
            e.capture_filter(text, &none),
            FilterOutcome::Accept,
            "{text}"
        );
    }
}

#[test]
fn off_switch_makes_write_validation_and_budget_inert() {
    let off = Engine::new(Policy::off());
    let c = supported("a.md").observed(day(10));
    assert_eq!(off.validate_write(&c.snap(), &ev(), day(1)), Ok(()));
    let counts = ActiveCounts {
        per_entity: [("a".into(), 1)].into(),
        total: 1,
    };
    let r = LossRequest {
        target: "t".into(),
        entities: vec!["a".into()],
        kind: LossKind::Archive,
    };
    let events = vec![LossEvent::from_request(&r, Actor::Agent, day(0))];
    let requests = [r];
    assert_eq!(
        off.budget_check(&requests, &events, &counts, day(0)),
        BudgetDecision::Allowed
    );
    // The policy-free helper and the lifecycle-on engine still enforce.
    assert!(matches!(
        budget_check(&requests, &events, &counts, day(0)),
        BudgetDecision::BudgetExhausted(_)
    ));
    let on = Engine::default();
    assert!(on.validate_write(&c.snap(), &ev(), day(1)).is_err());
}

#[test]
fn closed_work_reopens_only_when_the_condition_changes() {
    let c = supported("a.md").active();
    let s = Snapshot::new(vec![c.snap()], Evidence::default());
    let e = Engine::default();
    let first = e.sweep(&Context::new(&s, day(1)));
    let item = &first.create[0];
    assert!(item.condition.is_some());
    let closed = item
        .close(
            Outcome::EvidenceUnavailable,
            Some("source missing".into()),
            Some(Reopen {
                trigger: Trigger::SourceChange,
                detail: "source.md reappears".into(),
            }),
        )
        .unwrap();
    let work = NoteSnapshot::from_bytes(closed.path("work"), closed.render().as_bytes());
    let s = s.with_note(work);
    // Same snapshot, same time: an honest closure stands.
    let again = e.sweep(&Context::new(&s, day(1)));
    assert!(
        again.refresh.is_empty() && again.create.is_empty(),
        "{again:?}"
    );
    // A later session sweep with the source still missing: still closed.
    assert!(e.sweep(&Context::new(&s, day(2))).refresh.is_empty());
    // The source reappears with other bytes: the condition changed, so it reopens.
    let back = Snapshot::new(
        s.notes().to_vec(),
        Evidence::default().with(R, SourceState::present(b"rewritten".to_vec(), Some(day(2)))),
    );
    let reopened = e.sweep(&Context::new(&back, day(3)));
    assert_eq!(reopened.refresh.len(), 1);
    assert_eq!(reopened.refresh[0].1.state, State::Ready);
}

#[test]
fn closure_without_recorded_state_is_not_reopened_by_type() {
    let c = supported("a.md").active();
    let s = Snapshot::new(vec![c.snap()], Evidence::default());
    let e = Engine::default();
    let item = e.sweep(&Context::new(&s, day(1))).create[0].clone();
    let legacy = WorkItem {
        condition: None,
        ..item
    }
    .close(
        Outcome::EvidenceUnavailable,
        Some("source missing".into()),
        Some(Reopen {
            trigger: Trigger::SourceChange,
            detail: "source.md reappears".into(),
        }),
    )
    .unwrap();
    let s = s.with_note(NoteSnapshot::from_bytes(
        legacy.path("work"),
        legacy.render().as_bytes(),
    ));
    assert!(e.sweep(&Context::new(&s, day(5))).refresh.is_empty());
}
