mod support;
use serde_json::json;
use std::sync::Arc;
use support::ToolResultText;
use wi_mcp::{Engine, VaultLifecycle};
use wi_store::{OpenOptions, Vault};

#[test]
fn a_current_read_titles_memory_by_summary() {
    let tmp = tempfile::tempdir().unwrap();
    let (v, _) = Vault::init_with(
        tmp.path().join("vault"),
        OpenOptions {
            index_path: Some(tmp.path().join("index.db")),
        },
    )
    .unwrap();
    let e = Engine::new(v, Arc::new(VaultLifecycle));
    let source = format!(
        "Fact about tea.\n{}",
        "Large supporting paragraph.\n".repeat(2400)
    );
    let made = e.call(
        "wi_write",
        json!({"identifier":"source.md","content":source}),
    );
    assert_ne!(made.is_error, Some(true));
    let claim = "---\ntype: memory\nsummary: A compact tea claim\nsources: [source.md]\n---\nA compact tea claim.\n";
    let made = e
        .call("wi_write", json!({"identifier":"claim.md","content":claim}))
        .text_value()
        .unwrap();
    assert_eq!(
        made["data"]["lifecycle"]["adoption"]["outcome"], "active",
        "{made}"
    );
    let bytes = std::fs::read(e.vault().root().join("claim.md")).unwrap();
    let n = wi_note::Note::parse(&bytes).unwrap();
    let pins = n.frontmatter().unwrap().get("sources").unwrap();
    let read = e
        .call("wi_read", json!({"identifier":"claim.md"}))
        .text_value()
        .unwrap();
    println!("auto_pin={pins}; current_read={read}");
    assert_eq!(
        read["hits"].as_array().unwrap().len(),
        1,
        "an automatically completed short claim must remain deliverable; {read}"
    );
    assert_eq!(
        read["hits"][0]["title"], "A compact tea claim",
        "memory read should use summary"
    );
}
