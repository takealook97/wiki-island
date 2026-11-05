//! Supplied snapshot provenance is advisory and does not tighten adoption.
mod common;

use common::*;
use serde_json::json;
use wi_lifecycle::*;

#[test]
fn agent_only_snapshot_lint_is_advisory_and_does_not_block_adoption() {
    let bytes = b"source bytes";
    let reference = "captures/source.html";
    let note = Claim::fact("memory/provenance.md")
        .agent()
        .sources(vec![url_pin(reference, bytes, "https://example.com")])
        .snap();
    let mut source = SourceState::present(bytes, Some(day(0)));
    source
        .observed_by
        .insert(sha256(bytes), vec!["agent".into()]);
    let evidence = Evidence::default().with(reference, source);
    let snapshot = Snapshot::new(vec![note.clone()], evidence);
    let ctx = Context::new(&snapshot, day(1));
    let engine = Engine::default();
    let lint = engine.lint(&note, &ctx);
    assert!(lint.iter().any(|f| f.code == LintCode::SourceSelfReported));
    let AdoptionOutcome::Active { gates, .. } = engine.evaluate_adoption(&note, &ctx) else {
        panic!("the advisory finding must not block adoption");
    };
    assert!(gates.failed().is_empty());
}

#[test]
fn provenance_warning_requires_only_agent_observers_of_resolving_url_pins() {
    let bytes = b"source bytes";
    let reference = "captures/source.html";
    for (origins, agent, url, resolved, warning) in [
        (vec!["agent"], true, true, true, true),
        (vec!["agent", "user"], true, true, true, false),
        (vec!["unknown"], true, true, true, false),
        (vec!["user"], true, true, true, false),
        (vec!["agent"], false, true, true, false),
        (vec!["agent"], true, false, true, false),
        (vec!["agent"], true, true, false, false),
    ] {
        let source_pin = if url {
            url_pin(reference, bytes, "https://example.com")
        } else {
            pin(reference, bytes, Some((0, bytes.len())))
        };
        let claim = Claim::fact("memory/provenance.md").sources(vec![source_pin]);
        let note = if agent { claim.agent() } else { claim }.snap();
        let mut source = SourceState::present(
            if resolved {
                bytes.as_slice()
            } else {
                b"changed"
            },
            Some(day(0)),
        );
        source.observed_by.insert(
            sha256(bytes),
            origins.iter().map(|s| s.to_string()).collect(),
        );
        let snapshot = Snapshot::new(
            vec![note.clone()],
            Evidence::default().with(reference, source),
        );
        let lint = Engine::default().lint(&note, &Context::new(&snapshot, day(1)));
        assert_eq!(
            lint.iter().any(|f| f.code == LintCode::SourceSelfReported),
            warning,
            "{origins:?}, agent={agent}, url={url}, resolved={resolved}: {}",
            json!(lint)
        );
    }
}
