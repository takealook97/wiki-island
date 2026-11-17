//! `wi_search` ranks a note whose heading equals the query first, though many
//! other notes repeat the heading's words.
mod support;
use support::ToolResultText;

use std::sync::Arc;

use serde_json::json;
use tempfile::TempDir;
use wi_mcp::{Engine, VaultLifecycle};
use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

const REPEATING_NOTES: usize = 60;

fn put(vault: &Vault, path: String, content: String) {
    vault
        .write(WriteRequest {
            path,
            content: content.into_bytes(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Engine,
        })
        .unwrap();
}

#[test]
fn wi_search_ranks_a_heading_titled_note_first_among_repeating_notes() {
    let temp = TempDir::new().unwrap();
    let (vault, _) = Vault::init_with(
        temp.path().join("vault"),
        OpenOptions {
            index_path: Some(temp.path().join("title.db")),
        },
    )
    .unwrap();
    let t = "Amber garden";
    for k in 0..REPEATING_NOTES {
        put(
            &vault,
            format!("AI/Claude/c{k:02}.md"),
            format!("# {t} conversation {k}\n\n{t} {t} {t} talk about the {t} again\n"),
        );
    }
    put(
        &vault,
        "zz/ordinary-00.md".into(),
        format!("# {t}\n\nA short ordinary note.\n"),
    );
    vault.initialize_index().unwrap();
    let engine = Engine::new(vault, Arc::new(VaultLifecycle));
    let result = engine.call("wi_search", json!({"query": t}));
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let out = result.text_value().unwrap();
    assert_eq!(out["hits"][0]["path"], "zz/ordinary-00.md", "{out}");
}
