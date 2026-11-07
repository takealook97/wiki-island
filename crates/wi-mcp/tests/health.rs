//! Procedure health sizing against the real MCP delivery: JSON escaping,
//! Korean text and the resolved source spans a delivery attaches.
mod support;
use support::ToolResultText;

use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use wi_mcp::{Engine, VaultLifecycle};
use wi_store::{MaintainOptions, OpenOptions, Vault};

#[test]
fn procedure_health_flags_a_note_that_cannot_fit_the_mcp_result() {
    let temp = TempDir::new().unwrap();
    let (vault, _) = Vault::init_with(
        temp.path().join("vault"),
        OpenOptions {
            index_path: Some(temp.path().join("index.db")),
        },
    )
    .unwrap();
    // The Markdown file is below 16 KiB; JSON escaping doubles its body.
    let text = format!(
        "---\ntype: memory\nkind: procedure\nclaim_kind: intent\nskill_id: quoted-procedure\nsummary: Quote handling\nstatus: active\norigin: {{kind: user}}\nentities: []\nscope: work\n---\n{}\n",
        "\\\"".repeat(6000)
    );
    assert!(text.len() < wi_mcp::MAX_BYTES);
    std::fs::write(vault.root().join("procedure.md"), &text).unwrap();
    vault.auto_ingest().unwrap();
    vault.flush().unwrap();
    let report = vault.maintain(&MaintainOptions::default()).unwrap();
    let items = vault.work_list().unwrap();
    let e = Engine::new(vault, Arc::new(VaultLifecycle));
    let read = e
        .call("wi_read", json!({"identifier":"procedure.md"}))
        .text_value()
        .unwrap();
    println!(
        "file bytes: {}; maintenance: {report:?}; health items: {items:?}; read: {read}",
        text.len()
    );
    assert_eq!(
        read["hits"].as_array().unwrap().len(),
        0,
        "fixture must really exceed the MCP result budget"
    );
    assert!(
        items
            .iter()
            .any(|i| i.work_kind.as_deref() == Some("procedure")),
        "MCP-undeliverable procedure must get a health item"
    );
}

fn init() -> (TempDir, Vault) {
    let t = TempDir::new().unwrap();
    let (v, _) = Vault::init_with(
        t.path().join("vault"),
        OpenOptions {
            index_path: Some(t.path().join("index.db")),
        },
    )
    .unwrap();
    (t, v)
}
fn procedure(extra: &str, body: &str) -> String {
    format!(
        "---\ntype: memory\nkind: procedure\nclaim_kind: intent\nskill_id: size-procedure\nsummary: Size handling\nstatus: active\norigin: {{kind: user}}\nentities: []\nscope: work\n{extra}---\n{body}\n"
    )
}
#[test]
fn korean_and_escaped_body_estimates_cover_real_delivery() {
    for (kind, unit) in [("korean", "한글 근거 "), ("json", "\\\"\t\r\n")] {
        for n in [1, 300, 1000, 3000] {
            let (_t, v) = init();
            let text = procedure("", &unit.repeat(n));
            let estimate = wi_lifecycle::delivery_size("procedure.md", text.as_bytes());
            std::fs::write(v.root().join("procedure.md"), &text).unwrap();
            v.auto_ingest().unwrap();
            v.flush().unwrap();
            v.maintain(&MaintainOptions::default()).unwrap();
            let items = v.work_list().unwrap();
            let e = Engine::new(v, Arc::new(VaultLifecycle));
            let read = e
                .call("wi_read", json!({"identifier":"procedure.md"}))
                .text_value()
                .unwrap();
            let actual = serde_json::to_vec(&read).unwrap().len();
            let delivered = !read["hits"].as_array().unwrap().is_empty();
            println!(
                "{kind} n={n} file={} estimate={estimate} actual={actual} delivered={delivered}",
                text.len()
            );
            if delivered {
                assert!(estimate >= actual as u64);
            } else {
                assert!(
                    items
                        .iter()
                        .any(|i| i.work_kind.as_deref() == Some("procedure")),
                    "missing size item"
                );
            }
        }
    }
}
#[test]
fn health_accounts_for_resolved_source_spans() {
    let (_t, v) = init();
    let source = "한글 근거 ".repeat(3000);
    std::fs::create_dir_all(v.root().join("captures")).unwrap();
    std::fs::write(v.root().join("captures/source.txt"), &source).unwrap();
    let hash = wi_lifecycle::sha256(source.as_bytes());
    let extra = format!(
        "sources: [{{ref: captures/source.txt, commit: null, pending: true, source_hash: {hash}, span_hash: {hash}, locator: {{span: {{start: 0, end: {}}}}}}}]\n",
        source.len()
    );
    let text = procedure(&extra, "Use the supported procedure.");
    let estimate = wi_lifecycle::delivery_size("procedure.md", text.as_bytes());
    std::fs::write(v.root().join("procedure.md"), &text).unwrap();
    v.auto_ingest().unwrap();
    v.flush().unwrap();
    let labels = v
        .classify(
            &[("procedure.md".into(), text.as_bytes().to_vec())],
            &serde_json::Value::Null,
        )
        .unwrap();
    assert_eq!(labels[0].eligibility, "current");
    assert!(!labels[0].spans.is_empty());
    v.maintain(&MaintainOptions::default()).unwrap();
    let items = v.work_list().unwrap();
    let e = Engine::new(v, Arc::new(VaultLifecycle));
    let read = e
        .call("wi_read", json!({"identifier":"procedure.md"}))
        .text_value()
        .unwrap();
    println!(
        "file={} estimate={estimate} resolved span={} read={read} health items={items:?}",
        text.len(),
        source.len()
    );
    assert!(estimate < wi_lifecycle::MCP_MAX_BYTES);
    assert!(read["hits"].as_array().unwrap().is_empty());
    assert!(
        read["withheld"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["reason"] == "does-not-fit")
    );
    assert!(
        items
            .iter()
            .any(|i| i.work_kind.as_deref() == Some("procedure")),
        "undeliverable procedure with a resolved span must get a health item"
    );
}

#[test]
fn span_aware_estimate_covers_a_delivered_procedure_with_spans() {
    let (_t, v) = init();
    let source = "한글 근거 ".repeat(200);
    std::fs::create_dir_all(v.root().join("captures")).unwrap();
    std::fs::write(v.root().join("captures/source.txt"), &source).unwrap();
    let hash = wi_lifecycle::sha256(source.as_bytes());
    let extra = format!(
        "sources: [{{ref: captures/source.txt, commit: null, pending: true, source_hash: {hash}, span_hash: {hash}, locator: {{span: {{start: 0, end: {}}}}}}}]\n",
        source.len()
    );
    let text = procedure(&extra, "Use the supported procedure.");
    std::fs::write(v.root().join("procedure.md"), &text).unwrap();
    v.auto_ingest().unwrap();
    v.flush().unwrap();
    let labels = v
        .classify(
            &[("procedure.md".into(), text.as_bytes().to_vec())],
            &serde_json::Value::Null,
        )
        .unwrap();
    assert!(!labels[0].spans.is_empty());
    let estimate =
        wi_lifecycle::delivery_size_with_spans("procedure.md", text.as_bytes(), &labels[0].spans);
    v.maintain(&MaintainOptions::default()).unwrap();
    let items = v.work_list().unwrap();
    let e = Engine::new(v, Arc::new(VaultLifecycle));
    let read = e
        .call("wi_read", json!({"identifier":"procedure.md"}))
        .text_value()
        .unwrap();
    let actual = serde_json::to_vec(&read).unwrap().len() as u64;
    assert_eq!(read["hits"].as_array().unwrap().len(), 1, "{read}");
    assert!(estimate >= actual, "estimate {estimate} < actual {actual}");
    assert!(
        !items
            .iter()
            .any(|i| i.work_kind.as_deref() == Some("procedure")),
        "{items:?}"
    );
}
