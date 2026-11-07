//! Density ladder: plain notes that do not fit whole are presented at a
//! smaller computed tier instead of evicting other hits; lifecycle claims and
//! their qualifiers are delivered whole or withheld whole.
mod support;
use std::sync::Arc;
use support::ToolResultText;

use serde_json::{Value, json};
use tempfile::TempDir;
use wi_mcp::provider::{Lifecycle, Unavailable, VaultLifecycle};
use wi_mcp::{Engine, MAX_BYTES};
use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

fn fixture(provider: Arc<dyn Lifecycle>) -> (TempDir, Engine) {
    let temp = TempDir::new().unwrap();
    let (vault, _) = Vault::init_with(
        temp.path().join("vault"),
        OpenOptions {
            index_path: Some(temp.path().join("index.db")),
        },
    )
    .unwrap();
    (temp, Engine::new(vault, provider))
}

fn put(e: &Engine, path: &str, content: &str) {
    e.vault()
        .write(WriteRequest {
            path: path.into(),
            content: content.as_bytes().to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Engine,
        })
        .unwrap();
}

fn call(e: &Engine, name: &str, a: Value) -> Value {
    let result = e.call(name, a);
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let value = result.text_value().unwrap();
    assert!(serde_json::to_vec(&value).unwrap().len() <= MAX_BYTES);
    value
}

fn withheld(value: &Value, reason: &str) -> u64 {
    value["withheld"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["reason"] == reason)
        .map_or(0, |w| w["count"].as_u64().unwrap())
}

fn big_note() -> String {
    let mut text = String::from("Ledger payment guide. It covers every case.\n\n# Steps\n");
    text.push_str("1. Check the payment batch, EU accounts only\n");
    text.push_str("2. Reconcile before 17:00 UTC\n");
    text.push('\n');
    for i in 0..300 {
        text.push_str(&format!(
            "Background payment paragraph {i} with filler words.\n"
        ));
    }
    text
}

#[test]
fn one_big_note_no_longer_evicts_several_smaller_ones() {
    let (_temp, e) = fixture(Arc::new(Unavailable));
    put(&e, "big.md", &big_note());
    for i in 0..6 {
        put(
            &e,
            &format!("small-{i}.md"),
            &format!("Small payment note {i}. {}", "detail ".repeat(280)),
        );
    }
    let value = call(&e, "wi_search", json!({"query":"payment","limit":8}));
    let hits = value["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 7, "{value}");
    assert_eq!(withheld(&value, "does-not-fit"), 0, "{value}");
    let presented: Vec<_> = hits.iter().map(|h| h["presented"].clone()).collect();
    assert!(presented.iter().all(|p| p.is_null() || p.is_string()));
    let big = hits.iter().find(|h| h["path"] == "big.md").unwrap();
    assert_ne!(big["presented"], "full");
    // Kept lines are whole lines of the note, never cut inside a line.
    let original = big_note();
    for line in big["content"].as_str().unwrap().lines() {
        assert!(
            original.lines().any(|l| l == line) || big["presented"] == "summary",
            "{line}"
        );
    }
    // Same input, same output.
    let again = call(&e, "wi_search", json!({"query":"payment","limit":8}));
    assert_eq!(again["hits"], value["hits"]);
}

#[test]
fn core_tier_keeps_numbered_steps_with_their_qualifiers() {
    let (_temp, e) = fixture(Arc::new(Unavailable));
    put(&e, "big.md", &big_note());
    put(&e, "other.md", &format!("payment {}", "word ".repeat(2600)));
    let value = call(&e, "wi_search", json!({"query":"payment","limit":8}));
    let big = value["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["path"] == "big.md")
        .unwrap();
    assert_eq!(big["presented"], "core", "{value}");
    assert_eq!(
        big["content"],
        "# Steps\n1. Check the payment batch, EU accounts only\n2. Reconcile before 17:00 UTC\n"
    );
}

#[test]
fn qualifier_text_is_never_truncated() {
    let (_temp, e) = fixture(Arc::new(VaultLifecycle));
    let claim = format!(
        "---\ntype: memory\nkind: fact\nstatus: active\nsummary: Refunds are manual, EU only\nscope: EU only\n---\nRefund payment handling. {} Applies to EU accounts only, never US.\n",
        "context ".repeat(900)
    );
    put(&e, "claim.md", &claim);
    for i in 0..5 {
        put(
            &e,
            &format!("plain-{i}.md"),
            &format!("Plain payment note {i}. {}", "filler ".repeat(500)),
        );
    }
    let value = call(
        &e,
        "wi_search",
        json!({"query":"payment","limit":8,"read_mode":"all"}),
    );
    let hits = value["hits"].as_array().unwrap();
    let delivered = hits.iter().find(|h| h["path"] == "claim.md");
    match delivered {
        Some(hit) => {
            assert!(hit.get("presented").is_none());
            assert_eq!(
                hit["content"],
                wi_note::Note::parse(claim.as_bytes()).unwrap().body()
            );
            assert_eq!(hit["metadata"]["scope"], "EU only");
        }
        None => assert!(withheld(&value, "does-not-fit") >= 1, "{value}"),
    }
    assert!(hits.iter().any(|h| h["presented"].is_string()), "{value}");

    // A claim that cannot fit whole is withheld, not reduced.
    let huge = format!(
        "---\ntype: memory\nkind: fact\nstatus: active\nsummary: Big claim, EU only\n---\nPayment claim {} EU only.\n",
        "long ".repeat(5000)
    );
    put(&e, "huge-claim.md", &huge);
    let value = call(
        &e,
        "wi_read",
        json!({"identifier":"huge-claim.md","read_mode":"all"}),
    );
    assert_eq!(value["hits"], json!([]));
    assert_eq!(withheld(&value, "does-not-fit"), 1);
    assert!(!value.to_string().contains("EU only"));
}

#[test]
fn core_tier_keeps_the_exception_that_qualifies_a_rule() {
    let (_temp, e) = fixture(Arc::new(Unavailable));
    let mut note = String::from(
        "Payment procedure.\n# Rules\n1. Always deploy the payment change.\n   Except on Friday: never deploy the payment change.\n2. Record the payment outcome.\n\n",
    );
    for i in 0..300 {
        note.push_str(&format!(
            "Background payment details {i} with filler words.\n"
        ));
    }
    put(&e, "big.md", &note);
    put(&e, "other.md", &format!("payment {}", "word ".repeat(2600)));
    let value = call(&e, "wi_search", json!({"query":"payment","limit":8}));
    let big = value["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["path"] == "big.md")
        .unwrap();
    assert_eq!(big["presented"], "core", "{value}");
    assert_eq!(
        big["content"],
        "# Rules\n1. Always deploy the payment change.\n   Except on Friday: never deploy the payment change.\n2. Record the payment outcome.\n"
    );
}

#[test]
fn whole_hits_are_marked_full() {
    let (_temp, e) = fixture(Arc::new(Unavailable));
    put(&e, "a.md", "A short payment note.");
    let value = call(&e, "wi_read", json!({"identifier":"a.md"}));
    assert!(value["hits"][0].get("presented").is_none());
    assert_eq!(value["hits"][0]["content"], "A short payment note.");
}
