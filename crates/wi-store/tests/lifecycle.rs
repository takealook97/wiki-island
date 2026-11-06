//! Lifecycle integration through the write path, pinned-source reads and disabled mode.

mod common;

use std::rc::Rc;

use chrono::{TimeDelta, Utc};
use common::{Fixture, git};
use serde_json::{Map, Value, json};
use wi_store::{
    EditRequest, Hooks, Ingress, MoveRequest, Query, SearchOptions, StoreError, Vault,
    WriteRequest, WriteResult,
};

fn hash(bytes: &[u8]) -> String {
    wi_lifecycle::sha256(bytes)
}

fn time(t: chrono::DateTime<Utc>) -> String {
    wi_lifecycle::format_time(t)
}

/// A memory claim without engine-owned keys (any ingress may create it).
#[derive(Clone)]
struct Claim {
    fields: Map<String, Value>,
    body: String,
}

impl Claim {
    fn fact(summary: &str) -> Self {
        let fields = json!({
            "type": "memory",
            "kind": "fact",
            "claim_kind": "historical",
            "summary": summary,
            "entities": [{"id": "svc:payments", "label": "Payments", "source": null}],
            "scope": "work",
            "applicability": {"from": null, "until": null, "conditions": [], "supersedes": []},
            "observed_at": time(Utc::now() - TimeDelta::days(1)),
            "sources": [],
        });
        Self {
            fields: fields.as_object().cloned().unwrap(),
            body: format!("{summary}."),
        }
    }
    fn rule(summary: &str) -> Self {
        Self::fact(summary)
            .set("kind", json!("rule"))
            .set("claim_kind", json!("intent"))
    }
    fn set(self, key: &str, value: Value) -> Self {
        let mut fields = self.fields;
        fields.insert(key.into(), value);
        Self { fields, ..self }
    }
    fn pin(self, reference: &str, bytes: &[u8]) -> Self {
        self.set(
            "sources",
            // The whole source is the deliverable span (attachment rule).
            json!([{"ref": reference, "commit": null, "pending": true, "source_hash": hash(bytes),
                    "span_hash": hash(bytes), "locator": {"span": {"start": 0, "end": bytes.len()}}}]),
        )
    }
    fn slot(self, slot: &str, value: Value) -> Self {
        self.set("slot", json!(slot)).set("value", value)
    }
    fn supersedes(self, targets: &[&str]) -> Self {
        let mut app = self.fields["applicability"].clone();
        app["supersedes"] = json!(targets);
        self.set("applicability", app)
    }
    fn entity(self, id: &str) -> Self {
        self.set(
            "entities",
            json!([{"id": id, "label": null, "source": null}]),
        )
    }
    fn text(&self) -> String {
        let mut out = String::from("---\n");
        for (key, value) in &self.fields {
            out.push_str(&format!("{key}: {value}\n"));
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out.push('\n');
        out
    }
}

fn write(vault: &Vault, path: &str, content: &str, ingress: Ingress) -> WriteResult {
    vault
        .lifecycle_write(WriteRequest {
            path: path.into(),
            content: content.as_bytes().to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress,
        })
        .unwrap()
}

/// A plain source note; returns the bytes the vault holds.
fn source(vault: &Vault, path: &str, text: &str) -> Vec<u8> {
    write(vault, path, text, Ingress::Cli);
    std::fs::read(vault.root().join(path)).unwrap()
}

/// Seed an already active user claim directly on disk (as if synced).
fn seed_active(vault: &Vault, path: &str, claim: &Claim) {
    let claim = claim
        .clone()
        .set("status", json!("active"))
        .set(
            "origin",
            json!({"kind": "user", "locator": "file", "captured_at": time(Utc::now() - TimeDelta::days(1))}),
        );
    let full = vault.root().join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, claim.text()).unwrap();
    vault.reindex().unwrap();
}

fn field(vault: &Vault, path: &str, key: &str) -> Value {
    let bytes = std::fs::read(vault.root().join(path)).unwrap();
    let note = wi_note::Note::parse(&bytes).unwrap();
    note.frontmatter()
        .and_then(|f| f.get(key))
        .unwrap_or(Value::Null)
}

fn status(vault: &Vault, path: &str) -> Value {
    field(vault, path, "status")
}

fn eligibility(vault: &Vault, path: &str) -> (String, Option<String>) {
    let bytes = std::fs::read(vault.root().join(path)).unwrap();
    let labels = vault
        .classify(&[(path.into(), bytes)], &Value::Null)
        .unwrap();
    (labels[0].eligibility.clone(), labels[0].reason.clone())
}

fn work_notes(vault: &Vault, kind: &str) -> Vec<String> {
    vault
        .work_list()
        .unwrap()
        .into_iter()
        .filter(|w| w.work_kind.as_deref() == Some(kind))
        .map(|w| w.path)
        .collect()
}

fn outcome(result: &WriteResult) -> String {
    result.lifecycle.as_ref().unwrap()["adoption"]["outcome"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn cli_candidate_with_passing_gates_becomes_active_with_gates() {
    let (_f, vault) = Fixture::new();
    let bytes = source(
        &vault,
        "notes/runbook.md",
        "# Runbook\n\nThe port is 443.\n",
    );
    let claim = Claim::fact("The port is 443").pin("notes/runbook.md", &bytes);
    let result = write(&vault, "memory/port.md", &claim.text(), Ingress::Cli);
    assert_eq!(outcome(&result), "active", "{:?}", result.lifecycle);
    assert_eq!(status(&vault, "memory/port.md"), json!("active"));
    let adoption = field(&vault, "memory/port.md", "adoption");
    // All gates passed, so the note carries no gate block.
    assert!(adoption.get("gates").is_none(), "{adoption}");
    assert!(adoption["at"].is_string(), "{adoption}");
    let report = vault.adopt("memory/port.md").unwrap();
    assert_eq!(report.outcome, "unchanged", "{report:?}");
    assert_eq!(eligibility(&vault, "memory/port.md").0, "current");
}

#[test]
fn agent_rule_is_noted_as_a_plain_note_and_a_forged_active_rule_is_withheld() {
    let (_f, vault) = Fixture::new();
    let result = write(
        &vault,
        "memory/rule.md",
        &Claim::rule("Always deploy on Friday").text(),
        Ingress::Mcp,
    );
    // Never adopted (origin rule) and never an owner work item: kept as a
    // plain, labelled note instead of the requested claim.
    assert_eq!(outcome(&result), "noted");
    assert_eq!(result.path, "rules/rule.md");
    assert!(!vault.root().join("memory/rule.md").exists());
    assert_eq!(field(&vault, "rules/rule.md", "noted_by"), json!("agent"));
    assert_eq!(field(&vault, "rules/rule.md", "type"), Value::Null);
    assert!(work_notes(&vault, "origin-rule").is_empty());
    assert!(vault.current_rules().unwrap().rules.is_empty());
    // Forged status on disk: the read rule still withholds it.
    let forged = Claim::rule("Never deploy")
        .set("status", json!("active"))
        .set(
            "origin",
            json!({"kind": "agent", "locator": "mcp", "captured_at": null}),
        );
    std::fs::create_dir_all(vault.root().join("memory")).unwrap();
    std::fs::write(vault.root().join("memory/forged.md"), forged.text()).unwrap();
    assert_eq!(
        eligibility(&vault, "memory/forged.md"),
        ("withheld".into(), Some("origin-rule".into()))
    );
    // Adopting the forged note by hand never helps an agent rule.
    let again = vault.adopt("memory/forged.md").unwrap();
    assert_eq!(again.outcome, "unchanged");
}

#[test]
fn user_rule_adopts_and_a_file_edit_of_claim_content_resets_then_reruns_gates() {
    let (_f, vault) = Fixture::new();
    let result = write(
        &vault,
        "memory/rule.md",
        &Claim::rule("Deploy on Tuesday").text(),
        Ingress::Cli,
    );
    assert_eq!(outcome(&result), "active");
    assert_eq!(
        field(&vault, "memory/rule.md", "origin")["kind"],
        json!("user")
    );
    // An editor save changing the summary: file ingress sets user origin,
    // resets to candidate, then the gates run again.
    let path = vault.root().join("memory/rule.md");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace("Deploy on Tuesday", "Deploy on Wednesday"),
    )
    .unwrap();
    let report = vault.ingest_path("memory/rule.md").unwrap();
    let update = report.lifecycle_update.expect("status reset");
    assert!(update.checksum.starts_with("sha256:"));
    assert_eq!(report.adoption.expect("gates ran").outcome, "active");
    assert_eq!(status(&vault, "memory/rule.md"), json!("active"));
}

#[test]
fn active_conflict_flags_both_with_work_and_wi_work_opens_slotless_pairs() {
    let (_f, vault) = Fixture::new();
    let bytes = source(&vault, "notes/runbook.md", "# Runbook\n\nports\n");
    let a = Claim::fact("Port 443")
        .pin("notes/runbook.md", &bytes)
        .slot("port", json!(443));
    let b = Claim::fact("Port 8443")
        .pin("notes/runbook.md", &bytes)
        .slot("port", json!(8443));
    seed_active(&vault, "memory/a.md", &a);
    seed_active(&vault, "memory/b.md", &b);
    // Read-time detection withholds both before any flag is written.
    assert_eq!(
        eligibility(&vault, "memory/a.md").1.as_deref(),
        Some("conflict")
    );
    let summary = vault.sweep().unwrap();
    assert_eq!(summary.conflicts_flagged.len(), 2, "{summary:?}");
    assert_eq!(
        field(&vault, "memory/a.md", "conflict_with"),
        json!(["memory/b.md"])
    );
    assert_eq!(
        field(&vault, "memory/b.md", "conflict_with"),
        json!(["memory/a.md"])
    );
    assert_eq!(status(&vault, "memory/a.md"), json!("active"));
    assert_eq!(status(&vault, "memory/b.md"), json!("active"));
    assert_eq!(work_notes(&vault, "conflict").len(), 1);
    // No slot: no detection; wi_work opens a conflict item for the pair.
    seed_active(
        &vault,
        "memory/c.md",
        &Claim::fact("Region EU").pin("notes/runbook.md", &bytes),
    );
    seed_active(
        &vault,
        "memory/d.md",
        &Claim::fact("Region US").pin("notes/runbook.md", &bytes),
    );
    assert_eq!(eligibility(&vault, "memory/c.md").0, "current");
    let opened = vault
        .work_open_conflict("memory/c.md", "memory/d.md", "regions disagree")
        .unwrap();
    assert!(
        opened.starts_with(".wiki-island/work/conflict-"),
        "{opened}"
    );
    assert_eq!(work_notes(&vault, "conflict").len(), 2);
}

#[test]
fn changed_source_withholds_opens_recheck_and_sweep_keeps_status() {
    let (_f, vault) = Fixture::new();
    let bytes = source(
        &vault,
        "notes/runbook.md",
        "# Runbook\n\nThe port is 443.\n",
    );
    let claim = Claim::fact("The port is 443").pin("notes/runbook.md", &bytes);
    write(&vault, "memory/port.md", &claim.text(), Ingress::Cli);
    assert_eq!(eligibility(&vault, "memory/port.md").0, "current");
    vault
        .write(WriteRequest {
            path: "notes/runbook.md".into(),
            content: b"# Runbook\n\nThe port is 8443.\n".to_vec(),
            expected_checksum: Some(hash(&bytes)),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    assert_eq!(
        eligibility(&vault, "memory/port.md"),
        ("withheld".into(), Some("drift".into()))
    );
    let before = status(&vault, "memory/port.md");
    let summary = vault.sweep().unwrap();
    assert_eq!(summary.work_created.len(), 1, "{summary:?}");
    assert!(summary.work_created[0].starts_with(".wiki-island/work/recheck-"));
    assert_eq!(status(&vault, "memory/port.md"), before);
    // A second sweep refreshes nothing new and still changes no status.
    let again = vault.sweep().unwrap();
    assert!(again.work_created.is_empty());
    assert_eq!(status(&vault, "memory/port.md"), json!("active"));
}

#[test]
fn current_state_claim_without_observed_at_fails_lint() {
    let (_f, vault) = Fixture::new();
    let mut claim = Claim::fact("Port is open").set("claim_kind", json!("current-state"));
    claim.fields.remove("observed_at");
    write(&vault, "memory/open.md", &claim.text(), Ingress::Cli);
    let reports = vault.lint(Some("memory/open.md")).unwrap();
    let codes: Vec<String> = reports[0]
        .findings
        .iter()
        .map(|f| {
            serde_json::to_value(f.code)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert!(codes.iter().any(|c| c.contains("observed")), "{codes:?}");
    assert_eq!(status(&vault, "memory/open.md"), json!("candidate"));
}

/// Six active claims on one entity, each pinned and current.
fn six_claims(vault: &Vault) {
    let bytes = source(vault, "notes/procs.md", "# Procedures\n");
    for i in 0..6 {
        let claim = Claim::fact(&format!("Procedure {i}"))
            .set("kind", json!("procedure"))
            .entity("team:ops")
            .pin("notes/procs.md", &bytes);
        seed_active(vault, &format!("memory/p{i}.md"), &claim);
    }
}

fn archive(vault: &Vault, path: &str, ingress: Ingress) -> Result<WriteResult, StoreError> {
    vault.lifecycle_archive(path, None, None, ingress)
}

#[test]
fn two_agent_losses_then_budget_exhausted_queued_and_resumed() {
    let (_f, vault) = Fixture::new();
    six_claims(&vault);
    archive(&vault, "memory/p0.md", Ingress::Mcp).unwrap();
    let moved = vault
        .lifecycle_move(MoveRequest {
            from: "memory/p1.md".into(),
            to: "archive/memory/p1.md".into(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Mcp,
        })
        .unwrap();
    assert_eq!(
        moved.lifecycle.as_ref().unwrap()["losses_charged"],
        json!(1)
    );
    assert_eq!(status(&vault, "archive/memory/p1.md"), json!("archived"));
    let third = archive(&vault, "memory/p2.md", Ingress::Mcp).unwrap_err();
    let StoreError::BudgetExhausted {
        retry_at,
        spent,
        work,
    } = &third
    else {
        panic!("{third:?}");
    };
    assert_eq!(*spent, 2);
    assert!(retry_at.is_some());
    assert!(
        work.as_deref()
            .is_some_and(|w| w.starts_with(".wiki-island/work/deferred-"))
    );
    let by_move = vault
        .lifecycle_move(MoveRequest {
            from: "memory/p3.md".into(),
            to: "archive/memory/p3.md".into(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Mcp,
        })
        .unwrap_err();
    assert_eq!(by_move.code(), "budget-exhausted");
    assert!(vault.root().join("memory/p2.md").exists());
    // A user action is not capped.
    archive(&vault, "memory/p4.md", Ingress::Cli).unwrap();
    // The window is durable: commit trailers carry each loss.
    vault.flush().unwrap();
    let log = git(vault.root(), &["log", "--format=%B"]);
    assert_eq!(log.matches("Wi-Loss: ").count(), 2, "{log}");
    // A fresh handle (index deleted) still sees the spent budget.
    let reopened = Vault::open_with(vault.root(), Default::default()).unwrap();
    assert_eq!(
        archive(&reopened, "memory/p5.md", Ingress::Mcp)
            .unwrap_err()
            .code(),
        "budget-exhausted"
    );
    // After the window the queue resumes on its own.
    vault.set_lifecycle_clock(Some(Utc::now() + TimeDelta::hours(25)));
    let resumed = vault.resume_deferred().unwrap();
    assert!(!resumed.is_empty(), "queue did not resume");
    assert!(vault.root().join("archive/memory/p2.md").exists());
}

#[test]
fn mcp_claim_edit_of_a_user_rule_creates_an_agent_candidate_that_never_adopts() {
    let (_f, vault) = Fixture::new();
    write(
        &vault,
        "memory/rule.md",
        &Claim::rule("Review every Monday").text(),
        Ingress::Cli,
    );
    assert_eq!(status(&vault, "memory/rule.md"), json!("active"));
    let result = vault
        .lifecycle_edit(EditRequest {
            path: "memory/rule.md".into(),
            set: vec![("summary".into(), json!("Review every Friday"))],
            remove: vec![],
            body: None,
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Mcp,
        })
        .unwrap();
    let info = result.lifecycle.clone().unwrap();
    let candidate = info["claim_edit"]["candidate"].as_str().unwrap().to_owned();
    assert_eq!(result.path, candidate);
    assert_eq!(info["claim_edit"]["supersedes"], json!(["memory/rule.md"]));
    assert_ne!(info["claim_edit"]["outcome"], json!("active"));
    assert_eq!(field(&vault, &candidate, "origin")["kind"], json!("agent"));
    assert_eq!(
        field(&vault, &candidate, "applicability")["supersedes"],
        json!(["memory/rule.md"])
    );
    assert_eq!(
        field(&vault, "memory/rule.md", "summary"),
        json!("Review every Monday")
    );
    assert_eq!(eligibility(&vault, "memory/rule.md").0, "current");
    assert_eq!(work_notes(&vault, "origin-rule").len(), 1);
}

#[test]
fn conflicting_candidate_flags_itself_and_the_active_claim_stays_current() {
    let (_f, vault) = Fixture::new();
    let bytes = source(&vault, "notes/runbook.md", "# Runbook\n");
    seed_active(
        &vault,
        "memory/a.md",
        &Claim::fact("Port 443")
            .pin("notes/runbook.md", &bytes)
            .slot("port", json!(443)),
    );
    let b = Claim::fact("Port 8443")
        .pin("notes/runbook.md", &bytes)
        .slot("port", json!(8443));
    let result = write(&vault, "memory/b.md", &b.text(), Ingress::Cli);
    assert_eq!(outcome(&result), "candidate");
    assert_eq!(
        field(&vault, "memory/b.md", "conflict_with"),
        json!(["memory/a.md"])
    );
    assert_eq!(eligibility(&vault, "memory/a.md").0, "current");
    assert_eq!(work_notes(&vault, "conflict").len(), 1);
}

#[test]
fn replacement_adopts_and_supersedes_its_target() {
    let (_f, vault) = Fixture::new();
    let old = source(&vault, "notes/runbook.md", "# Runbook\n");
    let old_claim = Claim::fact("Port 443")
        .pin("notes/runbook.md", &old)
        .slot("port", json!(443))
        .set("observed_at", json!(time(Utc::now() - TimeDelta::days(5))));
    seed_active(&vault, "memory/a.md", &old_claim);
    let new = Claim::fact("Port 8443")
        .pin("notes/runbook.md", &old)
        .slot("port", json!(8443))
        .supersedes(&["memory/a.md"]);
    let result = write(&vault, "memory/b.md", &new.text(), Ingress::Cli);
    assert_eq!(outcome(&result), "active", "{:?}", result.lifecycle);
    assert_eq!(status(&vault, "memory/a.md"), json!("superseded"));
    assert_eq!(eligibility(&vault, "memory/b.md").0, "current");
}

#[test]
fn agent_fact_cannot_supersede_or_archive_an_unrelated_user_rule() {
    let (_f, vault) = Fixture::new();
    write(
        &vault,
        "memory/rule.md",
        &Claim::rule("Use squash merges").text(),
        Ingress::Cli,
    );
    let bytes = source(&vault, "notes/n.md", "# N\n");
    let agent = Claim::fact("Merges are rebased")
        .pin("notes/n.md", &bytes)
        .supersedes(&["memory/rule.md"]);
    let result = write(&vault, "memory/agent.md", &agent.text(), Ingress::Mcp);
    assert_eq!(outcome(&result), "candidate");
    assert_eq!(eligibility(&vault, "memory/rule.md").0, "current");
    assert_eq!(work_notes(&vault, "invalid-supersedes").len(), 1);
    let refused = archive(&vault, "memory/rule.md", Ingress::Mcp).unwrap_err();
    assert_eq!(refused.code(), "rule-retirement");
    let by_move = vault
        .lifecycle_move(MoveRequest {
            from: "memory/rule.md".into(),
            to: "archive/memory/rule.md".into(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Mcp,
        })
        .unwrap_err();
    assert_eq!(by_move.code(), "rule-retirement");
    assert!(vault.root().join("memory/rule.md").exists());
}

#[test]
fn two_publishers_spend_one_budget() {
    let (fixture, vault) = Fixture::new();
    let bytes = source(&vault, "notes/s.md", "# S\n");
    for i in 0..3 {
        let claim = Claim::fact(&format!("Claim {i}"))
            .entity("svc:one")
            .pin("notes/s.md", &bytes);
        seed_active(&vault, &format!("memory/c{i}.md"), &claim);
    }
    drop(vault);
    let root = fixture.root_input();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let (root, barrier) = (root.clone(), barrier.clone());
            let options = fixture.options();
            std::thread::spawn(move || {
                let vault = Vault::open_with(root, options).unwrap();
                barrier.wait();
                archive(&vault, &format!("memory/c{i}.md"), Ingress::Mcp).map(|_| ())
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let applied = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(applied, 1, "{results:?}");
    let refused = results.iter().find_map(|r| r.as_ref().err()).unwrap();
    assert_eq!(refused.code(), "budget-exhausted");
}

#[test]
fn url_snapshot_renews_only_through_a_host_fetch() {
    let (_f, vault) = Fixture::new();
    let day0 = Utc::now() - TimeDelta::days(30);
    vault.set_lifecycle_clock(Some(day0));
    let page = "Pricing: 10 EUR\n";
    let input = wi_store::SnapshotInput {
        url: "https://example.com/pricing".into(),
        path: "captures/pricing.html".into(),
        content: page.into(),
    };
    vault.record_snapshot(&input, Ingress::Mcp).unwrap();
    let claim = Claim::fact("Pricing is 10 EUR").set(
        "sources",
        json!([{"ref": "captures/pricing.html", "commit": null, "pending": true,
                "source_hash": hash(page.as_bytes()), "span_hash": hash(page.as_bytes()),
                "locator": {"span": {"start": 0, "end": page.len()}, "url": "https://example.com/pricing"}}]),
    ).set("claim_kind", json!("current-state")).set("observed_at", json!(time(day0)));
    write(&vault, "memory/pricing.md", &claim.text(), Ingress::Cli);
    vault.set_lifecycle_clock(Some(day0 + TimeDelta::days(3)));
    assert_eq!(eligibility(&vault, "memory/pricing.md").0, "current");
    vault.set_lifecycle_clock(Some(day0 + TimeDelta::days(7)));
    assert_eq!(
        eligibility(&vault, "memory/pricing.md").1.as_deref(),
        Some("ttl-expired")
    );
    // A day-6 host fetch with the same bytes is an observation and renews it.
    vault.set_lifecycle_clock(Some(day0 + TimeDelta::days(6)));
    vault.record_snapshot(&input, Ingress::Mcp).unwrap();
    vault.set_lifecycle_clock(Some(day0 + TimeDelta::days(7)));
    assert_eq!(eligibility(&vault, "memory/pricing.md").0, "current");
    // A changed page replaces the snapshot: the claim drifts.
    vault
        .record_snapshot(
            &wi_store::SnapshotInput {
                content: "Pricing: 12 EUR\n".into(),
                ..input.clone()
            },
            Ingress::Mcp,
        )
        .unwrap();
    assert_eq!(
        eligibility(&vault, "memory/pricing.md").1.as_deref(),
        Some("drift")
    );
}

#[test]
fn pinned_source_change_during_a_read_retries_and_withholds() {
    let (fixture, vault) = Fixture::new();
    let bytes = source(&vault, "notes/runbook.md", "# Runbook\n\nport 443\n");
    let claim = Claim::fact("Runbook port 443").pin("notes/runbook.md", &bytes);
    write(&vault, "memory/port.md", &claim.text(), Ingress::Cli);
    let mut reader = fixture.open();
    let writer = Rc::new(fixture.open());
    let original = hash(&bytes);
    let w = writer.clone();
    let once = Rc::new(std::cell::Cell::new(false));
    reader.set_hooks(Hooks {
        during_read: Some(Rc::new(move |_| {
            if !once.replace(true) {
                w.write(WriteRequest {
                    path: "notes/runbook.md".into(),
                    content: b"# Runbook\n\nport 8443\n".to_vec(),
                    expected_checksum: Some(original.clone()),
                    op_id: None,
                    ingress: Ingress::Cli,
                })
                .unwrap();
            }
        })),
        ..Hooks::default()
    });
    let query = Query::text("port");
    let found = reader
        .search_current(&query, &Value::Null, SearchOptions::default())
        .unwrap();
    let paths: Vec<String> = found
        .response
        .hits
        .iter()
        .map(|h| h.rel_path.to_string_lossy().into_owned())
        .collect();
    assert!(!paths.contains(&"memory/port.md".to_owned()), "{paths:?}");
    assert!(
        found.response.withheld.iter().any(|w| w.reason == "drift"),
        "{:?}",
        found.response.withheld
    );
    // A change on both attempts reports generation-changed.
    let mut both = fixture.open();
    let w2 = writer.clone();
    let counter = Rc::new(std::cell::Cell::new(0u32));
    let c = counter.clone();
    both.set_hooks(Hooks {
        during_read: Some(Rc::new(move |_| {
            let n = c.get();
            c.set(n + 1);
            w2.write(WriteRequest {
                path: format!("notes/extra-{n}.md"),
                content: b"# extra\n".to_vec(),
                expected_checksum: None,
                op_id: None,
                ingress: Ingress::Cli,
            })
            .unwrap();
        })),
        ..Hooks::default()
    });
    let found = both
        .search_current(&query, &Value::Null, SearchOptions::default())
        .unwrap();
    assert!(
        found
            .response
            .diagnostics
            .iter()
            .any(|d| d == "generation-changed")
    );
}

#[test]
fn lifecycle_off_makes_every_rule_inert() {
    let (fixture, vault) = Fixture::new();
    drop(vault);
    let config = fixture.root_input().join(".wiki-island/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}lifecycle = \"off\"\n")).unwrap();
    let vault = fixture.open();
    assert!(!vault.lifecycle_on());
    let rule = Claim::rule("Agent rule");
    let result = write(&vault, "memory/rule.md", &rule.text(), Ingress::Mcp);
    assert!(result.lifecycle.is_none());
    assert_eq!(status(&vault, "memory/rule.md"), Value::Null);
    assert!(work_notes(&vault, "origin-rule").is_empty());
    let found = vault
        .search_current(
            &Query::text("Agent"),
            &Value::Null,
            SearchOptions::default(),
        )
        .unwrap();
    assert_eq!(found.response.hits.len(), 1);
    assert_eq!(found.labels[0].eligibility, "plain");
    six_claims(&vault);
    for i in 0..4 {
        archive(&vault, &format!("memory/p{i}.md"), Ingress::Mcp).unwrap();
    }
    assert!(vault.sweep().unwrap().work_created.is_empty());
    assert!(vault.lint(None).unwrap().is_empty());
}

#[test]
fn superseded_stays_terminal_and_revert_forks_a_restoring_note() {
    let (_f, vault) = Fixture::new();
    let bytes = source(&vault, "notes/runbook.md", "# Runbook\n");
    let old = Claim::fact("Port 443")
        .pin("notes/runbook.md", &bytes)
        .slot("port", json!(443))
        .set("observed_at", json!(time(Utc::now() - TimeDelta::days(5))));
    write(&vault, "memory/a.md", &old.text(), Ingress::Cli);
    vault.flush().unwrap();
    let commit = vault.history("memory/a.md").unwrap()[0].commit.clone();
    let new = Claim::fact("Port 8443")
        .pin("notes/runbook.md", &bytes)
        .slot("port", json!(8443))
        .supersedes(&["memory/a.md"]);
    write(&vault, "memory/b.md", &new.text(), Ingress::Cli);
    assert_eq!(status(&vault, "memory/a.md"), json!("superseded"));
    // Gates never revive a terminal note.
    assert_eq!(vault.adopt("memory/a.md").unwrap().outcome, "unchanged");
    let result = vault
        .lifecycle_revert("memory/a.md", &commit, None, None, Ingress::Cli)
        .unwrap();
    assert_ne!(result.path, "memory/a.md");
    assert_eq!(
        field(&vault, &result.path, "restores"),
        json!({"ref": "memory/a.md", "commit": commit})
    );
    assert!(result.lifecycle.unwrap()["adoption"]["outcome"].is_string());
    assert_eq!(status(&vault, "memory/a.md"), json!("superseded"));
    // Through MCP the same revert is a fork, never an in-place restore.
    let mcp = vault
        .lifecycle_revert("memory/a.md", &commit, None, None, Ingress::Mcp)
        .unwrap_err();
    assert_eq!(mcp.code(), "exists");
}

fn snapshot_claim(vault: &Vault, summary: &str, ingress: Ingress) -> wi_store::SnapshotInput {
    let input = wi_store::SnapshotInput {
        url: "https://example.com/observation".into(),
        path: "captures/observation.html".into(),
        content: "The observed source says port 443.".into(),
    };
    vault.record_snapshot(&input, ingress).unwrap();
    let claim = Claim::fact(summary).set(
        "sources",
        json!([{"ref": input.path, "source_hash": hash(input.content.as_bytes()),
            "pending": true, "commit": null, "span_hash": hash(input.content.as_bytes()),
            "locator": {"url": input.url, "span": {"start": 0, "end": input.content.len()}}}]),
    );
    write(vault, "memory/observation.md", &claim.text(), Ingress::Mcp);
    input
}

fn snapshot_label(vault: &Vault) -> Value {
    let path = "memory/observation.md";
    let bytes = std::fs::read(vault.root().join(path)).unwrap();
    json!(
        vault
            .classify(&[(path.into(), bytes)], &Value::Null)
            .unwrap()[0]
    )
}

#[test]
fn snapshot_observation_records_ingress_and_returns_it() {
    let (_fixture, vault) = Fixture::new();
    let input = wi_store::SnapshotInput {
        url: "https://example.com/observation".into(),
        path: "captures/observation.html".into(),
        content: "source bytes".into(),
    };
    for (ingress, expected) in [
        (Ingress::Mcp, "agent"),
        (Ingress::Cli, "user"),
        (Ingress::Watcher, "user"),
    ] {
        let result = vault.record_snapshot(&input, ingress).unwrap();
        assert_eq!(result["observed_by"], expected);
        let sidecar: Value = serde_json::from_slice(
            &std::fs::read(
                vault
                    .root()
                    .join("captures/observation.html.wi-observed.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            sidecar["observations"].as_array().unwrap().last().unwrap()["observed_by"],
            expected
        );
    }
}

#[test]
fn agent_snapshot_remains_active_with_nonblocking_lint_and_source_metadata() {
    let (_fixture, vault) = Fixture::new();
    snapshot_claim(&vault, "A summary of the supplied source", Ingress::Mcp);
    assert_eq!(status(&vault, "memory/observation.md"), "active");
    let label = snapshot_label(&vault);
    assert_eq!(label["eligibility"], "current");
    assert_eq!(
        label["freshness"]["sources"][0]["observed_by"],
        json!(["agent"])
    );
    assert_eq!(label["spans"][0]["observed_by"], json!(["agent"]));
    let lint = json!(vault.lint(Some("memory/observation.md")).unwrap());
    assert!(
        lint[0]["findings"].as_array().unwrap().iter().any(|f| {
            f["code"] == "source-self-reported"
                && f["message"].as_str().unwrap().contains("did not fetch")
        }),
        "{lint}"
    );
}

#[test]
fn literal_snapshot_still_delivers_provenance_in_freshness() {
    let (_fixture, vault) = Fixture::new();
    snapshot_claim(&vault, "The observed source says port 443.", Ingress::Mcp);
    let label = snapshot_label(&vault);
    assert_eq!(label["eligibility"], "current");
    assert_eq!(label["spans"], json!([]));
    assert_eq!(
        label["freshness"]["sources"][0]["observed_by"],
        json!(["agent"])
    );
}

#[test]
fn old_snapshot_sidecar_defaults_observer_to_unknown_and_survives_append() {
    let (_fixture, vault) = Fixture::new();
    let input = snapshot_claim(&vault, "A summary of the supplied source", Ingress::Mcp);
    let sidecar = vault
        .root()
        .join("captures/observation.html.wi-observed.json");
    let mut old: Value = serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
    old["observations"][0]
        .as_object_mut()
        .unwrap()
        .remove("observed_by");
    std::fs::write(&sidecar, serde_json::to_vec(&old).unwrap()).unwrap();
    assert_eq!(
        snapshot_label(&vault)["freshness"]["sources"][0]["observed_by"],
        json!(["unknown"])
    );
    assert!(
        !json!(vault.lint(Some("memory/observation.md")).unwrap())
            .to_string()
            .contains("source-self-reported")
    );
    vault.record_snapshot(&input, Ingress::Cli).unwrap();
    let appended: Value = serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
    assert_eq!(appended["observations"][0]["observed_by"], "unknown");
    assert_eq!(appended["observations"][1]["observed_by"], "user");
    assert_eq!(
        snapshot_label(&vault)["freshness"]["sources"][0]["observed_by"],
        json!(["unknown", "user"])
    );
}

#[test]
fn user_observation_of_other_bytes_does_not_attest_the_pinned_snapshot() {
    let (_fixture, vault) = Fixture::new();
    let input = snapshot_claim(&vault, "A summary of the supplied source", Ingress::Mcp);
    vault
        .record_snapshot(
            &wi_store::SnapshotInput {
                content: "another revision".into(),
                ..input.clone()
            },
            Ingress::Cli,
        )
        .unwrap();
    vault.record_snapshot(&input, Ingress::Mcp).unwrap();
    assert_eq!(
        snapshot_label(&vault)["freshness"]["sources"][0]["observed_by"],
        json!(["agent"])
    );
    vault.record_snapshot(&input, Ingress::Cli).unwrap();
    assert_eq!(
        snapshot_label(&vault)["freshness"]["sources"][0]["observed_by"],
        json!(["agent", "user"])
    );
    assert!(
        !json!(vault.lint(Some("memory/observation.md")).unwrap())
            .to_string()
            .contains("source-self-reported")
    );
}
