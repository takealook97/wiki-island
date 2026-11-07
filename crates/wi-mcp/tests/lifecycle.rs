//! MCP tools over the wired lifecycle provider through `Engine::call`.
mod support;
use support::ToolResultText;

use std::process::Command;
use std::sync::Arc;

use serde_json::{Value, json};
use tempfile::TempDir;
use wi_mcp::provider::VaultLifecycle;
use wi_mcp::{Engine, MAX_BYTES};
use wi_store::{OpenOptions, Vault};

fn fixture() -> (TempDir, Engine) {
    let temp = TempDir::new().unwrap();
    let (vault, _) = Vault::init_with(
        temp.path().join("vault"),
        OpenOptions {
            index_path: Some(temp.path().join("index.db")),
        },
    )
    .unwrap();
    (temp, Engine::new(vault, Arc::new(VaultLifecycle)))
}

fn call(e: &Engine, name: &str, a: Value) -> (bool, Value) {
    let result = e.call(name, a);
    assert!(serde_json::to_vec(&result.text_value()).unwrap().len() <= MAX_BYTES);
    (result.is_error == Some(true), result.text_value().unwrap())
}

fn good(e: &Engine, name: &str, a: Value) -> Value {
    let (error, out) = call(e, name, a);
    assert!(!error, "{out}");
    out
}

fn sha(bytes: &[u8]) -> String {
    wi_lifecycle::sha256(bytes)
}

fn fact(summary: &str, source: &str, bytes: &[u8]) -> String {
    format!(
        "---\ntype: memory\nkind: fact\nclaim_kind: historical\nsummary: {summary:?}\nentities: []\nscope: work\napplicability: {{\"from\": null, \"until\": null, \"conditions\": [], \"supersedes\": []}}\nobserved_at: 2026-01-01T00:00:00Z\nsources: [{{\"ref\": \"{source}\", \"commit\": null, \"pending\": true, \"source_hash\": \"{hash}\", \"span_hash\": \"{hash}\", \"locator\": {{\"span\": {{\"start\": 0, \"end\": {len}}}}}}}]\n---\n{summary}.\n",
        hash = sha(bytes),
        len = bytes.len()
    )
}

fn source(e: &Engine, path: &str, text: &str) -> Vec<u8> {
    good(e, "wi_write", json!({"identifier": path, "content": text}));
    std::fs::read(e.vault().root().join(path)).unwrap()
}

#[test]
fn mcp_memory_note_has_agent_origin_actor_trailer_and_runs_the_gates() {
    let (_t, e) = fixture();
    let bytes = source(&e, "notes/runbook.md", "# Runbook\n\nport 443\n");
    let out = good(
        &e,
        "wi_write",
        json!({"identifier": "memory/port.md", "content": fact("Port 443", "notes/runbook.md", &bytes)}),
    );
    assert_eq!(
        out["data"]["lifecycle"]["adoption"]["outcome"],
        json!("active"),
        "{out}"
    );
    let read = good(&e, "wi_read", json!({"identifier": "memory/port.md"}));
    let hit = &read["hits"][0];
    assert_eq!(hit["metadata"]["origin"]["kind"], json!("agent"));
    assert_eq!(hit["metadata"]["status"], json!("active"));
    assert_eq!(
        hit["freshness"]["state"]["state"],
        json!("timeless"),
        "{hit}"
    );
    e.vault().flush().unwrap();
    let log = Command::new("git")
        .arg("-C")
        .arg(e.vault().root())
        .args(["log", "--format=%B"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("Wi-Actor: mcp"), "{log}");
    assert!(log.contains("Wi-Actor: engine"), "{log}");
}

#[test]
fn engine_owned_fields_are_refused_through_the_wired_provider() {
    let (_t, e) = fixture();
    let bytes = source(&e, "notes/s.md", "# S\n");
    let forged = fact("Forged", "notes/s.md", &bytes)
        .replace("type: memory", "type: memory\nstatus: active");
    let (error, out) = call(
        &e,
        "wi_write",
        json!({"identifier": "memory/f.md", "content": forged}),
    );
    assert!(error);
    assert_eq!(out["error"]["code"], json!("engine-owned-field"));
    assert!(!e.vault().root().join("memory/f.md").exists());
    good(
        &e,
        "wi_write",
        json!({"identifier": "memory/g.md", "content": fact("Genuine", "notes/s.md", &bytes)}),
    );
    let (error, out) = call(
        &e,
        "wi_edit",
        json!({"identifier": "memory/g.md", "command": "set_metadata", "metadata": {"status": "rejected"}}),
    );
    assert!(error);
    assert_eq!(out["error"]["code"], json!("engine-owned-field"));
}

#[test]
fn withheld_claims_are_counts_and_reasons_only() {
    let (_t, e) = fixture();
    let bytes = source(&e, "notes/runbook.md", "# Runbook\n\nzebra 443\n");
    good(
        &e,
        "wi_write",
        json!({"identifier": "memory/zebra.md", "content": fact("Zebra port 443", "notes/runbook.md", &bytes)}),
    );
    // Drift the source: the claim is withheld from current reads.
    let checksum = sha(&bytes);
    good(
        &e,
        "wi_write",
        json!({"identifier": "notes/runbook.md", "content": "# Runbook\n\nzebra 8443\n", "expected_checksum": checksum}),
    );
    let out = good(&e, "wi_search", json!({"query": "zebra"}));
    let text = out.to_string();
    let paths: Vec<&str> = out["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["path"].as_str())
        .collect();
    assert!(!paths.contains(&"memory/zebra.md"), "{out}");
    assert!(text.contains("drift"), "{out}");
    assert!(
        !text.contains("Zebra port 443"),
        "withheld text leaked: {out}"
    );
    // Historic mode labels it instead of hiding it.
    let historic = good(
        &e,
        "wi_search",
        json!({"query": "zebra", "read_mode": "historic"}),
    );
    assert!(historic.to_string().contains("memory/zebra.md"));
}

#[test]
fn lifecycle_tools_capture_adopt_lint_work_and_claim_edit_through_mcp() {
    let (_t, e) = fixture();
    let (error, out) = call(&e, "wi_capture", json!({"content": "AKIAABCDEFGHIJKLMNOP"}));
    assert!(error);
    assert_eq!(out["error"]["code"], json!("capture-dropped"));
    let captured = good(
        &e,
        "wi_capture",
        json!({"content": "The team said deploys move to Tuesday.", "title": "standup"}),
    );
    assert_eq!(captured["data"]["capture_status"], json!("pending"));
    let rule = "---\ntype: memory\nkind: rule\nclaim_kind: intent\nsummary: Deploy on Tuesday\nentities: []\nscope: work\napplicability: {\"from\": null, \"until\": null, \"conditions\": [], \"supersedes\": []}\n---\nDeploy on Tuesday.\n";
    // An agent rule is never adopted: it is kept as a plain noted note.
    let out = good(
        &e,
        "wi_write",
        json!({"identifier": "memory/rule.md", "content": rule}),
    );
    let adoption = &out["data"]["lifecycle"]["adoption"];
    assert_eq!(adoption["outcome"], json!("noted"), "{out}");
    assert_eq!(adoption["path"], json!("rules/rule.md"), "{out}");
    assert!(!e.vault().root().join("memory/rule.md").exists());
    let lint = good(&e, "wi_lint", json!({"identifier": "rules/rule.md"}));
    assert!(lint["data"]["reports"].is_array());
    let list = good(&e, "wi_work", json!({"command": "list"}));
    let items = list["data"]["items"].as_array().unwrap();
    assert!(
        items.iter().all(|i| i["work_kind"] != json!("origin-rule")),
        "{list}"
    );
    // Closing a task as unsupported needs its limits and a reopen trigger.
    // An agent claim citing a missing note stays a candidate with a task.
    let claim = "---\ntype: memory\nkind: fact\nsummary: Deploys need a ticket\nsources: [notes/missing.md]\n---\nDeploys need a ticket.\n";
    good(
        &e,
        "wi_write",
        json!({"identifier": "memory/claim.md", "content": claim}),
    );
    let list = good(&e, "wi_work", json!({"command": "list"}));
    let path = list["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["target"] == json!("memory/claim.md"))
        .and_then(|i| i["path"].as_str())
        .unwrap_or_else(|| panic!("a task for the candidate claim: {list}"))
        .to_owned();
    let (error, refused) = call(
        &e,
        "wi_work",
        json!({"command": "close", "identifier": path, "outcome": "not_supported"}),
    );
    assert!(error);
    assert_eq!(refused["error"]["code"], json!("work-close-refused"));
    good(
        &e,
        "wi_work",
        json!({"command": "close", "identifier": path, "outcome": "not_supported", "limits": "cannot be checked here", "reopen_trigger": "session", "reopen_detail": "the owner edits the file"}),
    );
    // A claim edit on a user-origin claim creates a superseding candidate.
    let bytes = source(&e, "notes/s.md", "# S\n");
    e.vault()
        .lifecycle_write(wi_store::WriteRequest {
            path: "memory/user.md".into(),
            content: fact("User fact", "notes/s.md", &bytes).into_bytes(),
            expected_checksum: None,
            op_id: None,
            ingress: wi_store::Ingress::Cli,
        })
        .unwrap();
    let edit = good(
        &e,
        "wi_edit",
        json!({"identifier": "memory/user.md", "command": "set_metadata", "metadata": {"summary": "User fact, revised"}}),
    );
    let claim = &edit["data"]["lifecycle"]["claim_edit"];
    assert_eq!(claim["supersedes"], json!(["memory/user.md"]), "{edit}");
    assert!(
        claim["candidate"]
            .as_str()
            .unwrap()
            .starts_with("memory/user-rev-")
    );
}

#[test]
fn next_work_selects_open_tasks_by_priority_and_bounds_the_compact_payload() {
    let (_temp, e) = fixture();
    let folder = e.vault().root().join(e.vault().config().work_dir());
    std::fs::create_dir_all(&folder).unwrap();
    for (name, kind, state, reason) in [
        ("a-deferred", "deferred", "ready", "Try the change later"),
        (
            "z-conflict",
            "conflict",
            "investigating",
            "Resolve disagreement",
        ),
        ("b-recheck", "recheck", "checkpointed", "Check the source"),
        ("closed", "conflict", "closed", "Already resolved"),
        ("large", "gate", "ready", &"long reason ".repeat(1000)),
    ] {
        let text = format!(
            "---\ntype: work\nwork_id: {name}\nwork_kind: {kind}\ncreated_at: 2026-11-01T00:00:00Z\nwork: {}\n---\nTask details.\n",
            json!({"state":state,"target":"claim.md","reason":reason,"next_action":"Read the note"})
        );
        std::fs::write(folder.join(format!("{name}.md")), text).unwrap();
    }
    e.vault().catch_up().unwrap();
    let result = good(&e, "wi_work", json!({"action":"next"}));
    let data = &result["data"];
    assert_eq!(data["items"].as_array().unwrap().len(), 3, "{result}");
    assert!(
        data["items"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("z-conflict.md")
    );
    assert!(
        data["items"][1]["path"]
            .as_str()
            .unwrap()
            .ends_with("b-recheck.md")
    );
    assert!(
        data["items"][2]["path"]
            .as_str()
            .unwrap()
            .ends_with("a-deferred.md")
    );
    assert_eq!(data["remaining"], 1);
    assert!(serde_json::to_vec(data).unwrap().len() <= 4096);
    assert!(
        data["items"][0]["checksum"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    let limited = good(
        &e,
        "wi_work",
        json!({"command":"next","max_items":1,"max_bytes":512}),
    );
    assert_eq!(limited["data"]["items"].as_array().unwrap().len(), 1);
    assert!(serde_json::to_vec(&limited["data"]).unwrap().len() <= 512);
    let tiny = good(&e, "wi_work", json!({"action":"next","max_bytes":64}));
    assert_eq!(tiny["data"]["items"].as_array().unwrap().len(), 1);
    assert!(
        serde_json::to_vec(&tiny["data"]).unwrap().len()
            <= tiny["data"]["max_bytes"].as_u64().unwrap() as usize
    );
}
#[test]
fn all_read_mode_and_legacy_mode_names_deliver_the_same_labelled_evidence() {
    let (_temp, e) = fixture();
    std::fs::write(
        e.vault().root().join("claim.md"),
        "---\ntype: memory\nstatus: candidate\nsummary: A claim\n---\nA claim needing review.\n",
    )
    .unwrap();
    let current = good(&e, "wi_read", json!({"identifier":"claim.md"}));
    assert!(current["hits"].as_array().unwrap().is_empty());
    for mode in ["all", "historic", "explore"] {
        let read = good(
            &e,
            "wi_read",
            json!({"identifier":"claim.md","read_mode":mode}),
        );
        assert_eq!(read["hits"][0]["path"], "claim.md");
        assert!(read["hits"][0]["metadata"]["wi_eligibility"].is_object());
    }
}

#[test]
fn oversized_first_work_item_is_delivered_compactly_without_starvation() {
    let (_temp, e) = fixture();
    let folder = e.vault().root().join(e.vault().config().work_dir());
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("large.md"), format!("---\ntype: work\nwork_id: task-large\nwork_kind: conflict\ncreated_at: 2026-11-01T00:00:00Z\nwork: {}\n---\nTask details.\n", json!({"state":"ready","target":"claim.md","reason":"reason ".repeat(2000),"next_action":"Read note"}))).unwrap();
    e.vault().catch_up().unwrap();
    for budget in [64, 512, 4096] {
        let result = good(&e, "wi_work", json!({"action":"next","max_bytes":budget}));
        let data = &result["data"];
        assert_eq!(data["items"].as_array().unwrap().len(), 1, "{result}");
        assert_eq!(data["items"][0]["key"], "task-large");
        assert_eq!(data["items"][0]["kind"], "conflict");
        assert!(
            data["items"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("large.md")
        );
        assert!(
            serde_json::to_vec(data).unwrap().len() <= data["max_bytes"].as_u64().unwrap() as usize
        );
    }
}

#[test]
fn memory_and_two_plain_hits_deliver_metadata_and_pins_once_with_fewer_bytes() {
    let (_temp, e) = fixture();
    e.vault().set_lifecycle_clock(Some(
        wi_lifecycle::parse_time("2026-11-07T00:00:00Z").unwrap(),
    ));
    let bytes = source(&e, "notes/drink.md", "# Drink\n\nUse oat milk.\n");
    source(&e, "notes/order.md", "# Order\n\nOrder oat milk.\n");
    good(
        &e,
        "wi_write",
        json!({"identifier":"memory/drink.md", "content":fact("Use oat milk", "notes/drink.md", &bytes)}),
    );
    let response = e.call("wi_search", json!({"query":"oat milk"}));
    assert_ne!(response.is_error, Some(true), "{response:?}");
    let out = response.text_value().unwrap();
    let hits = out["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3, "{out}");
    // Reconstruct the former wire layout from the same authoritative bytes:
    // full file in content, pins in metadata, and lifecycle aliases at top level.
    let mut previous = out.clone();
    for hit in previous["hits"].as_array_mut().unwrap() {
        let read = e.vault().read(hit["path"].as_str().unwrap()).unwrap();
        let note = wi_note::Note::parse(&read.bytes).unwrap();
        hit["content"] = json!(std::str::from_utf8(&read.bytes).unwrap());
        let fields = note.frontmatter().unwrap();
        if let Some(pins) = fields.get("sources") {
            hit["metadata"]["sources"] = pins;
        }
        for key in ["status", "claim_kind"] {
            if let Some(value) = fields.get(key) {
                hit[key] = value;
            }
        }
    }
    let mut previous_wire = serde_json::to_value(&response).unwrap();
    previous_wire["content"][0]["text"] = json!(previous.to_string());
    let before = serde_json::to_vec(&previous_wire).unwrap().len();
    let after = serde_json::to_vec(&response).unwrap().len();
    assert!(after * 100 < before * 80, "before={before}, after={after}");
    println!(
        "search wire bytes, same one-memory/two-note corpus: previous layout={before}, current={after}"
    );
    for hit in hits {
        assert!(!hit["content"].as_str().unwrap().contains("origin:"));
        assert!(hit["metadata"].get("sources").is_none());
        assert!(hit.get("status").is_none());
        assert!(hit.get("claim_kind").is_none());
        if hit["lifecycle"] == "memory" {
            assert_eq!(hit["content"], "Use oat milk.\n");
            assert_eq!(hit["metadata"]["status"], "active");
            assert!(!hit["sources"].is_null());
            assert!(!hit["content"].as_str().unwrap().contains("source_hash"));
        }
    }
}
