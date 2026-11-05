//! Lifecycle behavior, the `lifecycle: off` baseline and determinism.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use common::*;
use serde_json::json;
use wi_lifecycle::*;
use wi_note::{OriginKind, Status};

const RUNBOOK: &str = "notes/runbook.md";
const SRC: &[u8] = b"The payment service listens on port 443 (beta, EU only).\n";
const SPAN: (usize, usize) = (0, 56);

fn span_text() -> &'static str {
    std::str::from_utf8(&SRC[SPAN.0..SPAN.1]).unwrap()
}

fn evidence() -> Evidence {
    Evidence::default().with(RUNBOOK, SourceState::present(SRC, Some(day(0))))
}

/// A user fact pinned to the runbook span, ready to adopt.
fn supported(path: &str) -> Claim {
    Claim::fact(path).sources(vec![pin(RUNBOOK, SRC, Some(SPAN))])
}

fn engine() -> Engine {
    Engine::default()
}

fn gate_value(edit: &NoteEdit, gate: &str) -> serde_json::Value {
    edit.set["adoption"]["gates"][gate].clone()
}

fn work_kinds(work: &[WorkItem]) -> Vec<WorkKind> {
    work.iter().map(|w| w.kind).collect()
}

/// Apply edits to the claims they name and return the new snapshot.
fn apply(snapshot: &Snapshot, claims: &[Claim], edits: &[NoteEdit]) -> Snapshot {
    let notes = claims
        .iter()
        .map(|c| {
            let note = edits
                .iter()
                .filter(|e| e.path == c.path)
                .try_fold(c.note(), |n, e| e.apply(&n))
                .unwrap();
            NoteSnapshot::from_note(&c.path, &note)
        })
        .collect();
    Snapshot::new(notes, snapshot.evidence.clone())
}

#[test]
fn candidate_with_all_gates_passing_becomes_active_with_gate_results() {
    let claim = supported("notes/port.md").set("summary", json!(span_text()));
    let snapshot = Snapshot::new(vec![claim.snap()], evidence());
    let ctx = Context::new(&snapshot, day(1));
    let AdoptionOutcome::Active {
        edits,
        gates,
        losses,
    } = engine().evaluate_adoption(&claim.snap(), &ctx)
    else {
        panic!("expected active");
    };
    assert!(gates.failed().is_empty());
    assert!(losses.is_empty());
    assert_eq!(edits[0].set["status"], json!("active"));
    for gate in [
        "sources",
        "classification",
        "literal",
        "lint",
        "conflict",
        "origin",
    ] {
        assert!(gates.results[gate].passed(), "{gate}");
        // Passing gates are not written into the note.
        assert_eq!(gate_value(&edits[0], gate), json!(null), "{gate}");
    }
    let adopted = edits[0].apply(&claim.note()).unwrap();
    let lifecycle = adopted.lifecycle();
    assert_eq!(lifecycle.status, Some(Status::Active));
    let adoption = lifecycle.adoption.unwrap();
    assert_eq!(adoption.literal, Some(true));
    assert_eq!(adoption.at.as_deref(), Some("2026-01-02T00:00:00Z"));
    assert!(adoption.gates.is_none());
    // Only status and adoption change; the body survives byte for byte.
    assert!(adopted.body().contains("Body of notes/port.md."));
}

#[test]
fn agent_rule_or_preference_is_never_adopted_by_any_path() {
    let e = engine();
    for kind in ["rule", "preference"] {
        let claim = Claim::rule("rules/r.md").set("kind", json!(kind)).agent();
        let snapshot = Snapshot::new(vec![claim.snap()], Evidence::default());
        let ctx = Context::new(&snapshot, day(1));
        // Path 1: direct evaluation.
        let outcome = e.evaluate_adoption(&claim.snap(), &ctx);
        let AdoptionOutcome::Rejected {
            reason,
            work,
            edits,
        } = &outcome
        else {
            panic!("{kind}: {outcome:?}");
        };
        assert_eq!(*reason, RejectReason::OriginRule);
        assert_eq!(work_kinds(work), vec![WorkKind::OriginRule]);
        assert_eq!(edits[0].set["status"], json!("rejected"));
        // Path 2: an agent-written `status: active` is never current.
        let forged = claim.clone().active();
        let forged_snapshot = Snapshot::new(vec![forged.snap()], Evidence::default());
        let forged_ctx = Context::new(&forged_snapshot, day(1));
        assert_eq!(
            e.eligibility(&forged.snap(), &forged_ctx),
            Eligibility::Withheld(Withheld::OriginRule)
        );
        // Path 3: a revert performed by an agent.
        let reverted = e
            .restore_revision(
                &claim.note(),
                &claim.path,
                Some("abc"),
                OriginKind::Agent,
                day(1),
            )
            .unwrap();
        let reverted = NoteSnapshot::from_note("rules/r-restored.md", &reverted);
        assert!(matches!(
            e.evaluate_adoption(&reverted, &ctx),
            AdoptionOutcome::Rejected {
                reason: RejectReason::OriginRule,
                ..
            }
        ));
        // Path 4: a deferred-queue resume with reserved budget still runs the origin gate.
        let reserved = [LossEvent {
            at: day(1),
            target: "x".into(),
            entities: vec![],
            kind: LossKind::Supersede,
            actor: Actor::Agent,
        }];
        let resumed = Context::new(&snapshot, day(1)).with_reserved(&reserved);
        assert!(matches!(
            e.evaluate_adoption(&claim.snap(), &resumed),
            AdoptionOutcome::Rejected {
                reason: RejectReason::OriginRule,
                ..
            }
        ));
        // Path 5: file and url origins are not user either.
        for origin in ["file", "url"] {
            let imported = claim.clone().set(
                "origin",
                json!({"kind": origin, "locator": "import", "captured_at": null}),
            );
            assert!(matches!(
                e.evaluate_adoption(&imported.snap(), &ctx),
                AdoptionOutcome::Rejected {
                    reason: RejectReason::OriginRule,
                    ..
                }
            ));
        }
    }
}

#[test]
fn user_file_edit_is_adopted_and_claim_edit_resets_to_candidate() {
    let e = engine();
    let rule = Claim::rule("rules/r.md").body("Always pay invoices in EUR.");
    let snapshot = Snapshot::new(vec![rule.snap()], Evidence::default());
    let ctx = Context::new(&snapshot, day(1));
    let AdoptionOutcome::Active { edits, .. } = e.evaluate_adoption(&rule.snap(), &ctx) else {
        panic!("user rule should adopt");
    };
    let active_note = edits[0].apply(&rule.note()).unwrap();
    let active = NoteSnapshot::from_note(&rule.path, &active_note);
    assert_eq!(active.lifecycle.status, Some(Status::Active));

    // A direct edit of claim content sets user and resets to candidate.
    let edited_claim = rule
        .clone()
        .active()
        .set("summary", json!("Always pay invoices in USD."))
        .agent();
    let edited = edited_claim.snap();
    let edit = e.on_file_ingress(Some(&active), &edited, day(2)).unwrap();
    assert_eq!(edit.set["status"], json!("candidate"));
    assert_eq!(edit.set["origin"]["kind"], json!("user"));
    let reset = NoteSnapshot::from_note(&rule.path, &edit.apply(&edited_claim.note()).unwrap());
    assert_eq!(reset.lifecycle.status, Some(Status::Candidate));
    assert_eq!(reset.lifecycle.origin.unwrap().kind, OriginKind::User);
    // A body-only edit is not a claim revision.
    let body_only = NoteSnapshot::from_note(&rule.path, &active_note);
    assert_eq!(e.on_file_ingress(Some(&active), &body_only, day(2)), None);
}

fn port(path: &str, value: i64) -> Claim {
    supported(path).slot("port", json!(value))
}

#[test]
fn active_conflict_has_no_winner_and_slotless_pairs_need_wi_work() {
    let e = engine();
    // B is observed later and has a later name; neither matters.
    let a = port("notes/a.md", 443).active();
    let b = port("notes/b.md", 8443).active().observed(day(5));
    let snapshot = Snapshot::new(vec![a.snap(), b.snap()], evidence());
    let ctx = Context::new(&snapshot, day(6));
    let conflicts = e.conflicts(&snapshot);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(
        (conflicts[0].a.as_str(), conflicts[0].b.as_str()),
        ("notes/a.md", "notes/b.md")
    );
    let plan = e.propagate_conflicts(&ctx);
    assert_eq!(plan.edits.len(), 2);
    assert_eq!(plan.edits[0].set["conflict_with"], json!(["notes/b.md"]));
    assert_eq!(plan.edits[1].set["conflict_with"], json!(["notes/a.md"]));
    assert_eq!(work_kinds(&plan.work), vec![WorkKind::Conflict]);
    for note in snapshot.notes() {
        assert!(matches!(
            e.eligibility(note, &ctx),
            Eligibility::Withheld(Withheld::Conflict(_))
        ));
    }
    // Without a slot nothing is detected; wi_work opens the pair by hand.
    let a = supported("notes/a.md").active().set("value", json!(443));
    let b = supported("notes/b.md").active().set("value", json!(8443));
    let snapshot = Snapshot::new(vec![a.snap(), b.snap()], evidence());
    assert!(e.conflicts(&snapshot).is_empty());
    let item = e.open_conflict_work(
        "notes/b.md",
        "notes/a.md",
        "operator says these disagree",
        day(6),
    );
    assert_eq!(item.kind, WorkKind::Conflict);
    assert_eq!(
        (item.target.as_str(), item.related.clone()),
        ("notes/a.md", vec!["notes/b.md".to_string()])
    );
}

#[test]
fn changed_source_withholds_claim_opens_recheck_and_keeps_status() {
    let e = engine();
    let claim = supported("notes/port.md").active();
    let changed = Evidence::default().with(
        RUNBOOK,
        SourceState::present(b"port 8443".to_vec(), Some(day(0))),
    );
    let snapshot = Snapshot::new(vec![claim.snap()], changed);
    let ctx = Context::new(&snapshot, day(1));
    assert_eq!(
        e.eligibility(&claim.snap(), &ctx),
        Eligibility::Withheld(Withheld::Freshness(FreshnessState::Drifted(vec![
            RUNBOOK.into()
        ])))
    );
    let report = e.sweep(&ctx);
    assert_eq!(work_kinds(&report.create), vec![WorkKind::Recheck]);
    assert_eq!(report.create[0].next_trigger, Trigger::SourceChange);
    assert_eq!(
        snapshot.get("notes/port.md").unwrap().lifecycle.status,
        Some(Status::Active)
    );
}

#[test]
fn sweep_creates_or_refreshes_work_and_never_changes_status() {
    let e = engine();
    let drifted = supported("notes/drift.md").active();
    let stale = supported("notes/stale.md")
        .active()
        .set("sources", json!([]))
        .set("kind", json!("event"));
    let capture = Claim::fact("captures/c.md")
        .set("type", json!("capture"))
        .set("capture_status", json!("pending"));
    let changed = Evidence::default().with(
        RUNBOOK,
        SourceState::present(b"other".to_vec(), Some(day(0))),
    );
    let notes = vec![drifted.snap(), stale.snap(), capture.snap()];
    let statuses: Vec<_> = notes.iter().map(|n| n.lifecycle.status.clone()).collect();
    let snapshot = Snapshot::new(notes, changed.clone());
    let ctx = Context::new(&snapshot, day(40));
    let first = e.sweep(&ctx);
    assert_eq!(first.create.len(), 3);
    assert!(first.refresh.is_empty());
    let after: Vec<_> = snapshot
        .notes()
        .iter()
        .map(|n| n.lifecycle.status.clone())
        .collect();
    let mut sorted = statuses.clone();
    sorted.sort_by_key(|s| format!("{s:?}"));
    let mut after_sorted = after;
    after_sorted.sort_by_key(|s| format!("{s:?}"));
    assert_eq!(sorted, after_sorted);

    // The work notes now exist: a second sweep refreshes them.
    let mut notes: Vec<NoteSnapshot> = snapshot.notes().to_vec();
    notes.extend(
        first
            .create
            .iter()
            .map(|w| plain(&w.path("work"), &w.render())),
    );
    let with_work = Snapshot::new(notes.clone(), changed.clone());
    let second = e.sweep(&Context::new(&with_work, day(41)));
    assert!(second.create.is_empty());
    assert_eq!(second.refresh.len(), 3);

    // A closed item reopens only on its own trigger, and only when the
    // trigger condition is observed in a new state.
    let recheck = first
        .create
        .iter()
        .find(|w| w.target == "notes/drift.md")
        .unwrap();
    let wrong = recheck
        .close(
            Outcome::EvidenceUnavailable,
            Some("source rewritten".into()),
            Some(Reopen {
                trigger: Trigger::Query,
                detail: "a query misses".into(),
            }),
        )
        .unwrap();
    let right = recheck
        .close(
            Outcome::EvidenceUnavailable,
            Some("source rewritten".into()),
            Some(Reopen {
                trigger: Trigger::SourceChange,
                detail: "runbook changes".into(),
            }),
        )
        .unwrap();
    let changed_again = Evidence::default().with(
        RUNBOOK,
        SourceState::present(b"other again".to_vec(), Some(day(0))),
    );
    for (closed, evidence, expect) in [
        (&wrong, &changed_again, 0),
        (&right, &changed, 0),
        (&right, &changed_again, 1),
    ] {
        let replaced: Vec<NoteSnapshot> = notes
            .iter()
            .filter(|n| n.work_id.as_deref() != Some(recheck.id.as_str()))
            .cloned()
            .chain([plain(&closed.path("work"), &closed.render())])
            .collect();
        let report = e.sweep(&Context::new(
            &Snapshot::new(replaced, evidence.clone()),
            day(41),
        ));
        let refreshed = report
            .refresh
            .iter()
            .filter(|(_, w)| w.id == recheck.id)
            .count();
        assert_eq!(refreshed, expect);
    }
}

#[test]
fn current_state_claim_without_observed_at_fails_lint() {
    let e = engine();
    let claim = supported("notes/port.md").remove("observed_at");
    let snapshot = Snapshot::new(vec![claim.snap()], evidence());
    let ctx = Context::new(&snapshot, day(1));
    let codes: Vec<LintCode> = e
        .lint(&claim.snap(), &ctx)
        .into_iter()
        .map(|f| f.code)
        .collect();
    assert!(codes.contains(&LintCode::MissingObservedAt), "{codes:?}");
    let AdoptionOutcome::Candidate { gates, work, .. } = e.evaluate_adoption(&claim.snap(), &ctx)
    else {
        panic!("expected candidate");
    };
    assert!(
        matches!(&gates.results["lint"], GateResult::Fail(r) if r.contains("missing-observed-at"))
    );
    assert_eq!(work_kinds(&work), vec![WorkKind::Gate]);
}

fn procedure(n: usize) -> Claim {
    Claim::fact(&format!("procedures/p{n}.md"))
        .set("kind", json!("procedure"))
        .set("claim_kind", json!("intent"))
        .agent()
        .active()
}

/// Run `action` against a snapshot with ledger state; return the decision,
/// the updated snapshot and the ledger.
fn apply_loss(
    claims: &mut [Claim],
    ledger: &BudgetLedger,
    kind: LossKind,
    target: usize,
    actor: Actor,
    now: Timestamp,
) -> (LossDecision, BudgetLedger) {
    let snapshot = Snapshot::new(
        claims.iter().map(Claim::snap).collect(),
        Evidence::default(),
    );
    let ctx = Context::new(&snapshot, now).with_ledger(ledger);
    let action = LossAction {
        kind,
        target: claims[target].path.clone(),
        actor,
    };
    let decision = engine().authorize_loss(&action, &ctx);
    let mut next = ledger.clone();
    if let LossDecision::Allowed { edit, losses } = &decision {
        next = BudgetLedger::new(
            ledger
                .events()
                .iter()
                .cloned()
                .chain(losses.clone())
                .collect(),
        );
        if let Some(edit) = edit {
            for (key, value) in &edit.set {
                claims[target] = claims[target].clone().set(key, value.clone());
            }
        }
        if kind == LossKind::MoveToArchive {
            claims[target].path = format!("archive/{}", claims[target].path);
        }
    }
    (decision, next)
}

#[test]
fn six_active_claims_allow_two_agent_losses_per_day() {
    for third in [
        LossKind::Archive,
        LossKind::MoveToArchive,
        LossKind::Supersede,
        LossKind::Exclude,
        LossKind::Conflict,
    ] {
        let mut claims: Vec<Claim> = (0..6).map(procedure).collect();
        let ledger = BudgetLedger::default();
        let (first, ledger) = apply_loss(
            &mut claims,
            &ledger,
            LossKind::Archive,
            0,
            Actor::Agent,
            day(1),
        );
        assert!(matches!(first, LossDecision::Allowed { ref losses, .. } if losses.len() == 1));
        let (second, ledger) = apply_loss(
            &mut claims,
            &ledger,
            LossKind::MoveToArchive,
            1,
            Actor::Agent,
            at(1, 60),
        );
        assert!(matches!(second, LossDecision::Allowed { ref losses, .. } if losses.len() == 1));
        let (decision, after) =
            apply_loss(&mut claims, &ledger, third, 2, Actor::Agent, at(1, 120));
        let LossDecision::Deferred {
            exhausted,
            entry,
            work,
        } = decision
        else {
            panic!("{third:?}: expected budget-exhausted");
        };
        assert_eq!(exhausted.spent.len(), 2);
        assert_eq!(exhausted.retry_at, Some(at(2, 60)));
        assert_eq!(work.kind, WorkKind::Deferred);
        assert_eq!(work.deferred.as_ref(), Some(&entry));
        assert_eq!(after.events().len(), 2);
        // A user file edit is not capped and records nothing.
        let (user, after_user) =
            apply_loss(&mut claims, &ledger, third, 2, Actor::User, at(1, 180));
        assert!(matches!(user, LossDecision::Allowed { ref losses, .. } if losses.is_empty()));
        assert_eq!(after_user.events().len(), 2);
    }
}

#[test]
fn superseded_stays_terminal_and_revert_creates_a_restoring_note() {
    let e = engine();
    let old = supported("notes/old.md").set("status", json!("superseded"));
    let snapshot = Snapshot::new(vec![old.snap()], evidence());
    let ctx = Context::new(&snapshot, day(1));
    assert_eq!(
        e.evaluate_adoption(&old.snap(), &ctx),
        AdoptionOutcome::Unchanged {
            reason: UnchangedReason::Terminal("superseded".into())
        }
    );
    // A file edit of a terminal note does not revive it.
    let edited = old.clone().set("summary", json!("changed"));
    let edit = e
        .on_file_ingress(Some(&old.snap()), &edited.snap(), day(1))
        .unwrap();
    assert!(!edit.set.contains_key("status"));
    let restored = e
        .restore_revision(
            &old.note(),
            &old.path,
            Some("3f2a"),
            OriginKind::User,
            day(2),
        )
        .unwrap();
    let restored = NoteSnapshot::from_note("notes/old-restored.md", &restored);
    let restores = restored.lifecycle.restores.clone().unwrap();
    assert_eq!(
        (restores.reference.as_str(), restores.commit.as_deref()),
        ("notes/old.md", Some("3f2a"))
    );
    assert_eq!(restored.lifecycle.status, Some(Status::Candidate));
    let next = snapshot.with_note(restored.clone());
    assert!(matches!(
        e.evaluate_adoption(&restored, &Context::new(&next, day(2))),
        AdoptionOutcome::Active { .. }
    ));
    assert_eq!(
        next.get("notes/old.md").unwrap().lifecycle.status,
        Some(Status::Superseded)
    );
}

#[test]
fn mcp_edit_of_user_rule_creates_agent_candidate_that_never_adopts() {
    let e = engine();
    let rule = Claim::rule("rules/eur.md").active().body("Pay in EUR.");
    let changes: BTreeMap<String, serde_json::Value> =
        [("summary".to_string(), json!("Pay in USD."))].into();
    let new = e
        .mcp_claim_edit(&rule.note(), &rule.path, &changes, day(3))
        .unwrap();
    let candidate = NoteSnapshot::from_note("rules/eur-2.md", &new);
    assert_eq!(
        candidate.lifecycle.origin.clone().unwrap().kind,
        OriginKind::Agent
    );
    assert_eq!(candidate.supersedes(), vec!["rules/eur.md".to_string()]);
    assert_eq!(candidate.lifecycle.summary.as_deref(), Some("Pay in USD."));
    let snapshot = Snapshot::new(vec![rule.snap(), candidate.clone()], Evidence::default());
    let ctx = Context::new(&snapshot, day(3));
    let AdoptionOutcome::Rejected { reason, work, .. } = e.evaluate_adoption(&candidate, &ctx)
    else {
        panic!("expected rejection");
    };
    assert_eq!(reason, RejectReason::OriginRule);
    assert_eq!(work_kinds(&work), vec![WorkKind::OriginRule]);
    assert!(e.eligibility(&rule.snap(), &ctx).is_current());
    // Engine-owned fields stay unwritable through wi_edit.
    let forged: BTreeMap<String, serde_json::Value> =
        [("status".to_string(), json!("active"))].into();
    assert!(matches!(
        e.mcp_claim_edit(&rule.note(), &rule.path, &forged, day(3)),
        Err(RevisionError::EngineOwned(_))
    ));
}

#[test]
fn conflicting_candidate_flags_itself_and_active_claim_stays_current() {
    let e = engine();
    let active = port("notes/a.md", 443).active();
    let candidate = port("notes/b.md", 8443);
    let snapshot = Snapshot::new(vec![active.snap(), candidate.snap()], evidence());
    let ctx = Context::new(&snapshot, day(1));
    let AdoptionOutcome::Candidate { edits, work, .. } =
        e.evaluate_adoption(&candidate.snap(), &ctx)
    else {
        panic!("expected candidate");
    };
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].path, "notes/b.md");
    assert_eq!(edits[0].set["conflict_with"], json!(["notes/a.md"]));
    assert!(work_kinds(&work).contains(&WorkKind::Conflict));
    let after = apply(&snapshot, &[active.clone(), candidate], &edits);
    let ctx = Context::new(&after, day(1));
    assert!(
        e.eligibility(after.get("notes/a.md").unwrap(), &ctx)
            .is_current()
    );
}

#[test]
fn replacement_adopts_supersedes_target_and_recovers_after_a_crash() {
    let e = engine();
    let target = port("notes/a.md", 443).active();
    let candidate = port("notes/b.md", 8443)
        .agent()
        .observed(day(5))
        .supersedes(&["notes/a.md"]);
    let ev = Evidence::default().with(RUNBOOK, SourceState::present(SRC, Some(day(5))));
    let snapshot = Snapshot::new(vec![target.snap(), candidate.snap()], ev);
    let ledger = BudgetLedger::default();
    let ctx = Context::new(&snapshot, day(6)).with_ledger(&ledger);
    let AdoptionOutcome::Active { edits, losses, .. } =
        e.evaluate_adoption(&candidate.snap(), &ctx)
    else {
        panic!("expected active");
    };
    assert_eq!(edits[0].path, "notes/b.md");
    assert_eq!(edits[1].path, "notes/a.md");
    assert_eq!(edits[1].set["status"], json!("superseded"));
    assert_eq!(losses.len(), 1);
    assert_eq!(losses[0].kind, LossKind::Supersede);

    // Crash after the candidate write: the target is no longer current.
    let crashed = apply(&snapshot, &[target.clone(), candidate.clone()], &edits[..1]);
    let ctx = Context::new(&crashed, day(6));
    assert_eq!(
        e.eligibility(crashed.get("notes/a.md").unwrap(), &ctx),
        Eligibility::Withheld(Withheld::Superseded("notes/b.md".into()))
    );
    assert!(
        e.eligibility(crashed.get("notes/b.md").unwrap(), &ctx)
            .is_current()
    );
    let recovery = e.recover_supersedes(&ctx);
    assert_eq!(recovery, vec![edits[1].clone()]);
    let recovered = apply(&crashed, &[target, candidate], &edits);
    assert_eq!(
        recovered.get("notes/a.md").unwrap().lifecycle.status,
        Some(Status::Superseded)
    );
    assert!(
        e.recover_supersedes(&Context::new(&recovered, day(6)))
            .is_empty()
    );
}

#[test]
fn gate_classes_candidate_literal_and_rejected() {
    let e = engine();
    // Gate failures stay candidate with a work item. Agent origin: a user
    // claim is self-attested and completed.
    for claim in [
        Claim::fact("notes/nosrc.md").agent(),
        supported("notes/noent.md")
            .set("entities", json!(null))
            .agent(),
    ] {
        let snapshot = Snapshot::new(vec![claim.snap()], evidence());
        let outcome = e.evaluate_adoption(&claim.snap(), &Context::new(&snapshot, day(1)));
        let AdoptionOutcome::Candidate { work, edits, .. } = outcome else {
            panic!("{}: {outcome:?}", claim.path);
        };
        assert_eq!(work_kinds(&work), vec![WorkKind::Gate]);
        assert_eq!(edits[0].set["status"], json!("candidate"));
    }
    // Literal versus paraphrase.
    let literal = supported("notes/lit.md").set("summary", json!(span_text()));
    let paraphrase = supported("notes/para.md").set("summary", json!("Payments use port 443."));
    let snapshot = Snapshot::new(vec![literal.snap(), paraphrase.snap()], evidence());
    let ctx = Context::new(&snapshot, day(1));
    let literal_of = |c: &Claim| match e.evaluate_adoption(&c.snap(), &ctx) {
        AdoptionOutcome::Active { gates, edits, .. } => (gates.literal, edits[0].clone()),
        other => panic!("{other:?}"),
    };
    let (lit, _) = literal_of(&literal);
    let (para, para_edit) = literal_of(&paraphrase);
    assert!(lit);
    assert!(!para);
    let delivered = NoteSnapshot::from_note(
        &paraphrase.path,
        &para_edit.apply(&paraphrase.note()).unwrap(),
    );
    let spans = e.attached_spans(&delivered, &ctx);
    assert_eq!(spans.len(), 1);
    assert!(spans[0].text.contains("(beta, EU only)"));
    // Rejections: origin violation, duplicate body hash, unparseable.
    let agent_rule = Claim::rule("rules/r.md").agent();
    let original = supported("notes/one.md").active().body("Same body.");
    let duplicate = supported("notes/two.md").body("Same body.");
    let bad_shape = supported("notes/bad.md").set("entities", json!("svc"));
    let snapshot = Snapshot::new(vec![original.snap(), duplicate.snap()], evidence());
    let ctx = Context::new(&snapshot, day(1));
    let reason = |snap: &NoteSnapshot| match e.evaluate_adoption(snap, &ctx) {
        AdoptionOutcome::Rejected { reason, .. } => reason,
        other => panic!("{other:?}"),
    };
    assert_eq!(reason(&agent_rule.snap()), RejectReason::OriginRule);
    assert_eq!(
        reason(&duplicate.snap()),
        RejectReason::Duplicate("notes/one.md".into())
    );
    let bad = NoteSnapshot::from_note(&bad_shape.path, &bad_shape.note());
    assert!(matches!(reason(&bad), RejectReason::Unparseable(_)));
    let invalid_utf8 = NoteSnapshot::from_bytes("notes/bin.md", &[0xff, 0xfe, b'-']);
    assert!(matches!(
        reason(&invalid_utf8),
        RejectReason::Unparseable(_)
    ));
}

#[test]
fn freshness_window_renewal_ttl_and_future_timestamps() {
    let e = engine();
    let checked = supported("notes/port.md").active().set(
        "check",
        json!({"checked_at": format_time(day(25)), "result": "unchanged", "integrity_at": null, "volatile": false, "ttl": null}),
    );
    let snapshot = Snapshot::new(vec![checked.snap()], evidence());
    let fresh = e.freshness(&checked.snap(), &snapshot.evidence, at(54, 86_399));
    assert_eq!(fresh.state, FreshnessState::Fresh);
    assert_eq!(fresh.last_verified, Some(day(25)));
    assert_eq!(fresh.citable_until, Some(day(55)));
    assert_eq!(
        e.freshness(&checked.snap(), &snapshot.evidence, day(55))
            .state,
        FreshnessState::Stale
    );
    assert!(
        e.eligibility(&checked.snap(), &Context::new(&snapshot, at(54, 86_399)))
            .is_current()
    );
    assert!(
        !e.eligibility(&checked.snap(), &Context::new(&snapshot, day(55)))
            .is_current()
    );

    let ttl = checked.clone().set(
        "check",
        json!({"checked_at": format_time(day(25)), "result": "unchanged", "integrity_at": null, "volatile": true, "ttl": "P7D"}),
    );
    let f = e.freshness(&ttl.snap(), &snapshot.evidence, at(31, 86_399));
    assert_eq!(
        (f.state, f.citable_until),
        (FreshnessState::Fresh, Some(day(32)))
    );
    assert_eq!(
        e.freshness(&ttl.snap(), &snapshot.evidence, day(32)).state,
        FreshnessState::TtlExpired
    );

    let future = supported("notes/future.md").observed(day(10));
    assert!(matches!(
        e.validate_write(&future.snap(), &snapshot.evidence, day(9)),
        Err(FreshnessState::FutureTimestamp(_))
    ));
    assert!(
        e.validate_write(&future.snap(), &snapshot.evidence, day(10))
            .is_ok()
    );
    let future_active = future.active();
    let snapshot = Snapshot::new(vec![future_active.snap()], evidence());
    assert_eq!(
        e.eligibility(&future_active.snap(), &Context::new(&snapshot, day(9))),
        Eligibility::Withheld(Withheld::Freshness(FreshnessState::FutureTimestamp(
            "observed_at".into()
        )))
    );
}

const PAGE: &str = "snapshots/pricing.html";
const PAGE_BYTES: &[u8] = b"<p>Plan costs 10 EUR</p>";

fn url_claim() -> Claim {
    Claim::fact("notes/pricing.md")
        .set("origin", json!({"kind": "url", "locator": "https://example.com/pricing", "captured_at": format_time(day(0))}))
        .sources(vec![url_pin(PAGE, PAGE_BYTES, "https://example.com/pricing")])
        .active()
}

fn page(observations: Vec<Timestamp>) -> Evidence {
    Evidence::default().with(
        PAGE,
        SourceState::present(PAGE_BYTES, Some(day(0))).with_observations(observations),
    )
}

#[test]
fn url_claim_needs_a_snapshot_and_a_live_ttl_and_never_fetches() {
    let e = engine();
    let claim = url_claim();
    let with = Snapshot::new(vec![claim.snap()], page(vec![day(0)]));
    assert!(
        e.eligibility(&claim.snap(), &Context::new(&with, day(3)))
            .is_current()
    );
    assert_eq!(
        e.eligibility(&claim.snap(), &Context::new(&with, day(7))),
        Eligibility::Withheld(Withheld::Freshness(FreshnessState::TtlExpired))
    );
    let without = Snapshot::new(vec![claim.snap()], Evidence::default());
    assert_eq!(
        e.eligibility(&claim.snap(), &Context::new(&without, day(1))),
        Eligibility::Withheld(Withheld::Freshness(FreshnessState::NoSnapshot(vec![
            PAGE.into()
        ])))
    );
    // The crate has no network client: a query cannot fetch.
    let manifest = include_str!("../Cargo.toml");
    for client in ["reqwest", "hyper", "ureq", "curl", "isahc", "surf"] {
        assert!(!manifest.contains(client), "{client}");
    }
}

#[test]
fn active_note_under_archive_is_not_current() {
    let e = engine();
    let claim = supported("x").active().at_path("archive/notes/port.md");
    let snapshot = Snapshot::new(vec![claim.snap()], evidence());
    assert_eq!(
        e.eligibility(&claim.snap(), &Context::new(&snapshot, day(1))),
        Eligibility::Withheld(Withheld::Archived)
    );
}

#[test]
fn agent_fact_cannot_supersede_or_archive_an_unrelated_user_rule() {
    let e = engine();
    let rule = Claim::rule("rules/eur.md").active();
    let fact = supported("notes/f.md")
        .agent()
        .supersedes(&["rules/eur.md"]);
    let snapshot = Snapshot::new(vec![rule.snap(), fact.snap()], evidence());
    let ctx = Context::new(&snapshot, day(1));
    let AdoptionOutcome::Candidate { gates, work, .. } = e.evaluate_adoption(&fact.snap(), &ctx)
    else {
        panic!("expected candidate");
    };
    assert!(
        matches!(&gates.results["supersedes"], GateResult::Fail(r) if r.contains("rule-retirement"))
    );
    assert!(work_kinds(&work).contains(&WorkKind::InvalidSupersedes));
    assert!(e.eligibility(&rule.snap(), &ctx).is_current());
    for kind in [
        LossKind::Archive,
        LossKind::MoveToArchive,
        LossKind::Exclude,
        LossKind::Supersede,
    ] {
        let action = LossAction {
            kind,
            target: rule.path.clone(),
            actor: Actor::Agent,
        };
        assert!(
            matches!(
                e.authorize_loss(&action, &ctx),
                LossDecision::Refused { .. }
            ),
            "{kind:?}"
        );
        let engine_action = LossAction {
            actor: Actor::Engine,
            ..action.clone()
        };
        assert!(matches!(
            e.authorize_loss(&engine_action, &ctx),
            LossDecision::Refused { .. }
        ));
        let user = LossAction {
            actor: Actor::User,
            ..action
        };
        assert!(matches!(
            e.authorize_loss(&user, &ctx),
            LossDecision::Allowed { .. }
        ));
    }
}

#[test]
fn older_evidence_does_not_supersede_a_newer_target() {
    let e = engine();
    let target = port("notes/a.md", 443).active().set(
        "check",
        json!({"checked_at": format_time(day(10)), "result": "unchanged", "integrity_at": null, "volatile": false, "ttl": null}),
    );
    let candidate = port("notes/b.md", 8443)
        .observed(day(3))
        .supersedes(&["notes/a.md"]);
    let snapshot = Snapshot::new(vec![target.snap(), candidate.snap()], evidence());
    let ctx = Context::new(&snapshot, day(11));
    assert_eq!(
        e.validate_supersedes(&candidate.snap(), &target.snap(), &ctx),
        Err(InvalidTarget::OlderEvidence)
    );
    let AdoptionOutcome::Candidate { work, .. } = e.evaluate_adoption(&candidate.snap(), &ctx)
    else {
        panic!("expected candidate");
    };
    assert!(work_kinds(&work).contains(&WorkKind::InvalidSupersedes));
    assert!(e.eligibility(&target.snap(), &ctx).is_current());
}

#[test]
fn inflated_observed_at_is_lowered_to_the_snapshot_capture_time() {
    let e = engine();
    let target = port("notes/a.md", 443).active().observed(day(10));
    let candidate = port("notes/b.md", 8443)
        .agent()
        .observed(day(12))
        .sources(vec![url_pin(PAGE, PAGE_BYTES, "https://example.com/ports")])
        .supersedes(&["notes/a.md"]);
    let ev = evidence().with(
        PAGE,
        SourceState::present(PAGE_BYTES, Some(day(3))).with_observations(vec![day(3)]),
    );
    let snapshot = Snapshot::new(vec![target.snap(), candidate.snap()], ev);
    let ctx = Context::new(&snapshot, day(12));
    assert_eq!(
        e.effective_observed_at(&candidate.snap(), &snapshot.evidence),
        Ok(Some(day(3)))
    );
    assert_eq!(
        e.validate_supersedes(&candidate.snap(), &target.snap(), &ctx),
        Err(InvalidTarget::OlderEvidence)
    );
    assert!(matches!(
        e.evaluate_adoption(&candidate.snap(), &ctx),
        AdoptionOutcome::Candidate { .. }
    ));
    assert!(e.eligibility(&target.snap(), &ctx).is_current());
}

fn counts(entity: &str, active: usize) -> ActiveCounts {
    ActiveCounts {
        per_entity: [(entity.to_string(), active)].into(),
        total: active,
    }
}

fn loss(entity: &str, target: &str) -> LossRequest {
    LossRequest {
        target: target.into(),
        entities: vec![entity.into()],
        kind: LossKind::Archive,
    }
}

#[test]
fn window_boundary_and_backward_clock() {
    let e = engine();
    let t = day(5);
    let events = vec![LossEvent::from_request(&loss("svc", "a"), Actor::Agent, t)];
    // Three active before the first loss, two after it.
    let after = counts("svc", 2);
    let second = [loss("svc", "b")];
    let BudgetDecision::BudgetExhausted(exhausted) = e.budget_check(
        &second,
        &events,
        &after,
        t + chrono::TimeDelta::seconds(86_399),
    ) else {
        panic!("expected budget-exhausted one second before the boundary");
    };
    assert_eq!(exhausted.retry_at, Some(day(6)));
    assert_eq!(exhausted.spent, events);
    assert_eq!(
        e.budget_check(&second, &events, &after, day(6)),
        BudgetDecision::Allowed
    );
    // The clock is set back before t: the loss still counts.
    assert!(matches!(
        e.budget_check(&second, &events, &after, t - chrono::TimeDelta::hours(1)),
        BudgetDecision::BudgetExhausted(_)
    ));
}

#[test]
fn concurrent_publishers_spend_one_entity_budget() {
    // Each thread is a publisher; the mutex stands in for the publisher lock.
    let ledger = Arc::new(Mutex::new(BudgetLedger::default()));
    let queue = Arc::new(Mutex::new(DeferredQueue::default()));
    let handles: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|target| {
            let ledger = Arc::clone(&ledger);
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                let mut guard = ledger.lock().unwrap();
                let request = [loss("svc", target)];
                match guard.reserve(&request, Actor::Agent, &counts("svc", 3), day(1)) {
                    Ok((next, _)) => {
                        *guard = next;
                        true
                    }
                    Err(_) => {
                        let entry = DeferredEntry {
                            id: target.into(),
                            action: DeferredAction::Loss,
                            requests: request.to_vec(),
                            actor: Actor::Agent,
                            enqueued_at: day(1),
                            newer_evidence: false,
                            target_hash: None,
                        };
                        let mut q = queue.lock().unwrap();
                        *q = q.push(entry);
                        false
                    }
                }
            })
        })
        .collect();
    let applied: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(applied.iter().filter(|a| **a).count(), 1);
    assert_eq!(ledger.lock().unwrap().events().len(), 1);
    assert_eq!(queue.lock().unwrap().entries().len(), 1);

    // No vault-wide cap: four entities with room each all fit.
    let all = ActiveCounts {
        per_entity: ["e1", "e2", "e3", "e4"]
            .iter()
            .map(|e| (e.to_string(), 9))
            .collect(),
        total: 36,
    };
    let mut ledger = BudgetLedger::default();
    let mut outcomes = Vec::new();
    for entity in ["e1", "e2", "e3", "e4"] {
        match ledger.reserve(&[loss(entity, entity)], Actor::Engine, &all, day(1)) {
            Ok((next, _)) => {
                ledger = next;
                outcomes.push(None);
            }
            Err(exhausted) => outcomes.push(Some(exhausted.scopes)),
        }
    }
    assert_eq!(outcomes, [None, None, None, None]);
}

#[test]
fn starvation_is_visible_and_newer_evidence_goes_first() {
    let e = engine();
    // Six active claims on one entity; an agent spends the allowance on day 1.
    let target = port("notes/target.md", 443).active();
    let others: Vec<Claim> = (0..5).map(|n| procedure(n).active()).collect();
    let ev = Evidence::default().with(RUNBOOK, SourceState::present(SRC, Some(day(1))));
    let spent: Vec<LossEvent> = ["procedures/gone-1.md", "procedures/gone-2.md"]
        .iter()
        .enumerate()
        .map(|(i, t)| {
            LossEvent::from_request(&loss("svc:payments", t), Actor::Agent, at(1, i as i64))
        })
        .collect();
    let ledger = BudgetLedger::new(spent.clone());
    // An older-evidence entry waits in the queue first.
    let older = DeferredEntry {
        id: "older".into(),
        action: DeferredAction::Loss,
        requests: vec![loss("svc:payments", "procedures/p0.md")],
        actor: Actor::Agent,
        enqueued_at: at(1, 60),
        newer_evidence: false,
        target_hash: None,
    };
    let candidate = port("notes/new.md", 8443)
        .agent()
        .observed(day(1))
        .supersedes(&["notes/target.md"]);
    let mut claims = vec![target.clone(), candidate.clone()];
    claims.extend(others);
    let snapshot = Snapshot::new(claims.iter().map(Claim::snap).collect(), ev);
    let ctx = Context::new(&snapshot, at(1, 120)).with_ledger(&ledger);
    let AdoptionOutcome::Deferred {
        exhausted,
        entry,
        work,
        ..
    } = e.evaluate_adoption(&candidate.snap(), &ctx)
    else {
        panic!("expected budget-exhausted");
    };
    assert_eq!(exhausted.spent, spent);
    assert_eq!(exhausted.retry_at, Some(at(2, 0)));
    assert!(entry.newer_evidence);
    assert_eq!(work[0].kind, WorkKind::Deferred);
    let queue = DeferredQueue::default().push(older).push(entry.clone());
    assert_eq!(queue.entries()[0].id, entry.id);
    // The queue is rebuilt from the work notes alone.
    let work_note = plain(&work[0].path("work"), &work[0].render());
    let rebuilt =
        DeferredQueue::from_snapshot(&Snapshot::new(vec![work_note], Evidence::default()));
    assert_eq!(rebuilt.entries(), std::slice::from_ref(&entry));

    // Not yet at the retry time: nothing applies.
    let counts = ActiveCounts::from_snapshot(&snapshot, &e.policy);
    let early = queue.resume(&ledger, &counts, at(1, 86_399));
    assert!(early.ready.is_empty());
    // At the retry time the replacement applies first; the older entry still waits.
    let resumed = queue.resume(&ledger, &counts, at(2, 0));
    assert_eq!(resumed.ready.len(), 1);
    assert_eq!(resumed.ready[0].0.id, entry.id);
    assert_eq!(resumed.remaining.entries()[0].id, "older");
    let reserved = resumed.ready[0].1.clone();
    let ctx = Context::new(&snapshot, at(2, 1))
        .with_ledger(&resumed.ledger)
        .with_reserved(&reserved);
    let AdoptionOutcome::Active { losses, .. } = e.evaluate_adoption(&candidate.snap(), &ctx)
    else {
        panic!("expected the resumed replacement to adopt");
    };
    assert_eq!(losses, reserved);
}

#[test]
fn snapshot_renews_only_through_a_host_fetch() {
    let e = engine();
    // Day 6 re-hash of the unchanged snapshot: integrity only.
    let rehashed = url_claim().set(
        "check",
        json!({"checked_at": format_time(day(6)), "result": "unchanged", "integrity_at": format_time(day(6)), "volatile": true, "ttl": "P7D"}),
    );
    let no_fetch = Snapshot::new(vec![rehashed.snap()], page(vec![day(0)]));
    assert!(
        e.eligibility(&rehashed.snap(), &Context::new(&no_fetch, at(6, 3600)))
            .is_current()
    );
    assert_eq!(
        e.eligibility(&rehashed.snap(), &Context::new(&no_fetch, day(7))),
        Eligibility::Withheld(Withheld::Freshness(FreshnessState::TtlExpired))
    );
    // Day 6 host fetch with the same bytes: an observation event renews it.
    let fetched = Snapshot::new(vec![rehashed.snap()], page(vec![day(0), day(6)]));
    let f = e.freshness(&rehashed.snap(), &fetched.evidence, day(7));
    assert_eq!(
        (f.state, f.last_verified),
        (FreshnessState::Fresh, Some(day(6)))
    );
    assert!(
        e.eligibility(&rehashed.snap(), &Context::new(&fetched, day(12)))
            .is_current()
    );
    // A failed fetch records nothing: the day-0 observation stays.
    let failed = Snapshot::new(vec![rehashed.snap()], page(vec![day(0)]));
    assert_eq!(
        e.freshness(&rehashed.snap(), &failed.evidence, day(7))
            .last_verified,
        Some(day(0))
    );
}

fn mixed_vault() -> Vec<NoteSnapshot> {
    vec![
        port("notes/a.md", 443).active().snap(),
        port("notes/b.md", 8443).active().snap(),
        supported("notes/c.md").active().observed(day(-100)).snap(),
        Claim::rule("rules/r.md").agent().active().snap(),
        plain(
            "notes/plain.md",
            "---\ntitle: Plain\nstatus: active\n---\nLegacy note.\n",
        ),
        plain("notes/bare.md", "No frontmatter at all.\n"),
    ]
}

#[test]
fn v_off_lifecycle_off_reproduces_the_plain_baseline() {
    let off = Engine::new(Policy::off());
    let snapshot = Snapshot::new(mixed_vault(), evidence());
    let ctx = Context::new(&snapshot, day(1));
    // Baseline: every note served as a plain note, nothing withheld.
    let baseline = ReadPartition {
        current: vec![],
        plain: snapshot.notes().iter().map(|n| n.path.clone()).collect(),
        withheld: BTreeMap::new(),
    };
    assert_eq!(off.partition_current(&ctx), baseline);
    for note in snapshot.notes() {
        assert_eq!(
            off.eligibility(note, &ctx),
            Eligibility::Plain {
                lifecycle: wi_note::LifecycleType::None
            }
        );
        assert_eq!(
            off.evaluate_adoption(note, &ctx),
            AdoptionOutcome::Unchanged {
                reason: UnchangedReason::LifecycleOff
            }
        );
        assert!(off.lint(note, &ctx).is_empty());
    }
    assert_eq!(off.sweep(&ctx), SweepReport::default());
    assert!(off.conflicts(&snapshot).is_empty());
    assert_eq!(off.propagate_conflicts(&ctx), ConflictPlan::default());
    assert!(off.recover_supersedes(&ctx).is_empty());
    assert_eq!(
        off.capture_filter("password = hunter2hunter2", &BTreeSet::new()),
        FilterOutcome::Accept
    );
    // With lifecycle on, the same vault differs: the baseline is not trivially equal.
    let on = engine().partition_current(&ctx);
    assert_eq!(
        on.plain,
        vec!["notes/bare.md".to_string(), "notes/plain.md".to_string()]
    );
    assert_eq!(on.withheld["conflict"], 2);
    assert_eq!(on.withheld["origin-rule"], 1);
    assert_eq!(on.withheld["stale"], 1);
}

#[test]
fn plain_notes_are_lifecycle_none_and_legacy_status_is_ignored() {
    let e = engine();
    let legacy = plain(
        "notes/plain.md",
        "---\ntitle: Plain\nstatus: active\nkind: rule\n---\nBody\n",
    );
    let snapshot = Snapshot::new(vec![legacy.clone()], Evidence::default());
    let ctx = Context::new(&snapshot, day(1));
    assert_eq!(
        e.eligibility(&legacy, &ctx),
        Eligibility::Plain {
            lifecycle: wi_note::LifecycleType::None
        }
    );
    assert_eq!(
        e.evaluate_adoption(&legacy, &ctx),
        AdoptionOutcome::Unchanged {
            reason: UnchangedReason::NotMemory
        }
    );
}

#[test]
fn conditions_and_applicability_window_gate_current_reads() {
    let e = engine();
    let claim = supported("notes/eu.md")
        .active()
        .applicability(
            "conditions",
            json!([{"key": "region", "op": "in", "values": ["eu", "uk"]}]),
        )
        .applicability("from", ts(day(1)))
        .applicability("until", ts(day(30)));
    let snapshot = Snapshot::new(vec![claim.snap()], evidence());
    let at_day = |n, q: ReadQuery| {
        e.eligibility(
            &claim.snap(),
            &Context::new(&snapshot, day(n)).with_query(q),
        )
    };
    let eu = ReadQuery::default().with("region", json!("eu"));
    assert!(at_day(2, eu.clone()).is_current());
    assert_eq!(
        at_day(0, eu.clone()),
        Eligibility::Withheld(Withheld::NotYetApplicable)
    );
    assert_eq!(
        at_day(30, eu),
        Eligibility::Withheld(Withheld::ApplicabilityExpired)
    );
    assert_eq!(
        at_day(2, ReadQuery::default()),
        Eligibility::Withheld(Withheld::UnknownCondition("region".into()))
    );
    assert_eq!(
        at_day(2, ReadQuery::default().with("region", json!("us"))),
        Eligibility::Withheld(Withheld::ConditionUnmet("region".into()))
    );
    // Disjoint conditions never conflict.
    let us = port("notes/us.md", 80).active().applicability(
        "conditions",
        json!([{"key": "region", "op": "eq", "values": ["us"]}]),
    );
    let eu_port = port("notes/eup.md", 443).active().applicability(
        "conditions",
        json!([{"key": "region", "op": "eq", "values": ["eu"]}]),
    );
    assert!(
        e.conflicts(&Snapshot::new(vec![us.snap(), eu_port.snap()], evidence()))
            .is_empty()
    );
}

#[test]
fn work_items_close_honestly_and_round_trip_as_notes() {
    let item = WorkItem::new(
        WorkKind::Recheck,
        "notes/a.md",
        vec![],
        "drift",
        "recheck",
        Trigger::SourceChange,
        day(1),
    );
    assert!(matches!(
        item.close(Outcome::NotSupported, None, None),
        Err(WorkError::UnsupportedOutcomeNeedsLimits(
            Outcome::NotSupported
        ))
    ));
    assert!(item.close(Outcome::Adopted, None, None).is_ok());
    assert_eq!(item.checkpoint("out of time").state, State::Checkpointed);
    let note = plain(&item.path("work"), &item.render());
    assert_eq!(note.lifecycle.lifecycle_type, wi_note::LifecycleType::Work);
    assert_eq!(note.work_id.as_deref(), Some(item.id.as_str()));
    let work = note.lifecycle.work.unwrap();
    assert_eq!(work.target.as_deref(), Some("notes/a.md"));
    assert_eq!(work.state, Some(wi_note::WorkState::Ready));
    assert!(item.path("work").starts_with("work/recheck-"));
}

#[test]
fn outputs_are_deterministic_and_order_independent() {
    let e = engine();
    let notes = mixed_vault();
    let reversed: Vec<NoteSnapshot> = notes.iter().rev().cloned().collect();
    let one = Snapshot::new(notes, evidence());
    let two = Snapshot::new(reversed, evidence());
    let (c1, c2) = (Context::new(&one, day(3)), Context::new(&two, day(3)));
    assert_eq!(e.partition_current(&c1), e.partition_current(&c2));
    assert_eq!(e.conflicts(&one), e.conflicts(&two));
    let json = |v: &dyn erased::Json| v.json();
    assert_eq!(json(&e.sweep(&c1)), json(&e.sweep(&c2)));
    assert_eq!(
        json(&e.propagate_conflicts(&c1)),
        json(&e.propagate_conflicts(&c2))
    );
    for (a, b) in one.notes().iter().zip(two.notes()) {
        assert_eq!(
            json(&e.evaluate_adoption(a, &c1)),
            json(&e.evaluate_adoption(b, &c2))
        );
        assert_eq!(e.lint(a, &c1), e.lint(b, &c2));
    }
}

mod erased {
    pub trait Json {
        fn json(&self) -> String;
    }
    impl<T: serde::Serialize> Json for T {
        fn json(&self) -> String {
            serde_json::to_string(self).unwrap()
        }
    }
}
