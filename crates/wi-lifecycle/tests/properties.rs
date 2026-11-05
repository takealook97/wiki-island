//! Property tests for the change-cap arithmetic and window boundaries.

use chrono::{TimeDelta, TimeZone, Utc};
use proptest::prelude::*;
use wi_lifecycle::*;

fn t0() -> Timestamp {
    Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap()
}

fn request(entity: &str) -> LossRequest {
    LossRequest {
        target: "t".into(),
        entities: vec![entity.into()],
        kind: LossKind::Archive,
    }
}

fn event(entity: &str, offset_seconds: i64) -> LossEvent {
    LossEvent::from_request(
        &request(entity),
        Actor::Agent,
        t0() + TimeDelta::seconds(offset_seconds),
    )
}

/// One entity with `active` claims.
fn counts(active: usize) -> ActiveCounts {
    ActiveCounts {
        per_entity: [("e".to_string(), active)].into(),
        total: 1_000_000,
    }
}

const DAY: i64 = 86_400;

proptest! {
    #[test]
    fn per_entity_decision_matches_the_formula(active in 0usize..80, losses in 0usize..40) {
        // Every loss sits inside the window.
        let events: Vec<LossEvent> = (0..losses).map(|i| event("e", i as i64)).collect();
        let now = t0() + TimeDelta::seconds(losses as i64);
        // losses_in_window + 1 <= max(1, floor((active_now + losses_in_window) / 3))
        let allowed = losses < std::cmp::max(1, (active + losses) / 3);
        let decision = budget_check(&[request("e")], &events, &counts(active), now);
        prop_assert_eq!(decision == BudgetDecision::Allowed, allowed);
    }

    #[test]
    fn an_event_counts_exactly_while_now_is_before_its_end(offset in -3 * DAY..3 * DAY) {
        let e = event("e", 0);
        let now = t0() + TimeDelta::seconds(offset);
        // Before the event (clock moved back) and inside the window it counts.
        prop_assert_eq!(e.in_window(now), offset < DAY);
    }

    #[test]
    fn retry_at_is_the_earliest_time_the_action_fits(
        active in 1usize..30,
        offsets in prop::collection::vec(0i64..DAY, 1..12),
    ) {
        let events: Vec<LossEvent> = offsets.iter().map(|o| event("e", *o)).collect();
        let now = t0() + TimeDelta::seconds(DAY - 1);
        let c = counts(active);
        if let BudgetDecision::BudgetExhausted(exhausted) = budget_check(&[request("e")], &events, &c, now) {
            prop_assert!(!exhausted.spent.is_empty());
            if let Some(retry) = exhausted.retry_at {
                prop_assert!(retry > now);
                prop_assert_eq!(budget_check(&[request("e")], &events, &c, retry), BudgetDecision::Allowed);
                let before = retry - TimeDelta::seconds(1);
                prop_assert_ne!(budget_check(&[request("e")], &events, &c, before), BudgetDecision::Allowed);
            }
        }
    }

    #[test]
    fn decisions_do_not_depend_on_event_order(
        active in 0usize..30,
        offsets in prop::collection::vec(-DAY..2 * DAY, 0..12),
        seed in any::<u64>(),
    ) {
        let events: Vec<LossEvent> = offsets.iter().map(|o| event("e", *o)).collect();
        let mut shuffled = events.clone();
        let len = shuffled.len().max(1);
        shuffled.rotate_left((seed as usize) % len);
        shuffled.reverse();
        let now = t0() + TimeDelta::seconds(DAY / 2);
        let c = counts(active);
        prop_assert_eq!(
            budget_check(&[request("e")], &events, &c, now),
            budget_check(&[request("e")], &shuffled, &c, now)
        );
    }

    #[test]
    fn sequential_reservations_never_exceed_the_starting_allowance(active in 1usize..60) {
        // Each applied loss removes one active claim, as an archive would.
        let mut ledger = BudgetLedger::default();
        let mut c = counts(active);
        let mut applied = 0usize;
        for i in 0..active {
            let now = t0() + TimeDelta::seconds(i as i64);
            match ledger.reserve(&[request("e")], Actor::Agent, &c, now) {
                Ok((next, _)) => {
                    ledger = next;
                    c = c.after(&[request("e")]);
                    applied += 1;
                }
                Err(_) => break,
            }
        }
        prop_assert_eq!(applied, std::cmp::max(1, active / 3));
    }

    #[test]
    fn there_is_no_vault_wide_cap_across_entities(total in 0usize..400, n in 1usize..40) {
        let mut ledger = BudgetLedger::default();
        let c = ActiveCounts {
            per_entity: (0..n).map(|i| (format!("e{i}"), 1_000)).collect(),
            total,
        };
        let mut applied = 0;
        for i in 0..n {
            if let Ok((next, _)) = ledger.reserve(&[request(&format!("e{i}"))], Actor::Engine, &c, t0()) {
                ledger = next;
                applied += 1;
            }
        }
        prop_assert_eq!(applied, n);
    }

    #[test]
    fn user_actions_are_never_capped(active in 0usize..10, losses in 0usize..20) {
        let events: Vec<LossEvent> = (0..losses).map(|i| event("e", i as i64)).collect();
        let ledger = BudgetLedger::new(events);
        let result = ledger.reserve(&[request("e")], Actor::User, &counts(active), t0());
        prop_assert!(matches!(result, Ok((_, ref added)) if added.is_empty()));
    }

    #[test]
    fn iso_durations_add_fixed_lengths(days in 0i64..400, hours in 0i64..48) {
        let d = IsoDuration::parse(&format!("P{days}DT{hours}H")).unwrap();
        prop_assert_eq!(d.after(t0()), Some(t0() + TimeDelta::seconds(days * DAY + hours * 3600)));
    }
}
