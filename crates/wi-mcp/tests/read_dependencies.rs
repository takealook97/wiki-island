//! A publication during a read withholds the result only when it touches
//! what the read returned: the hits, their qualifiers or a lifecycle note.
mod support;
use support::ToolResultText;

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use serde_json::{Value, json};
use tempfile::TempDir;
use wi_mcp::Engine;
use wi_mcp::provider::Unavailable;
use wi_store::{Hooks, Ingress, OpenOptions, Vault, WriteRequest};

fn options(temp: &Path) -> OpenOptions {
    OpenOptions {
        index_path: Some(temp.join("index.db")),
    }
}

fn put(vault: &Vault, path: &str, content: String) {
    let expected = vault.read(path).ok().map(|r| r.checksum);
    vault
        .write(WriteRequest {
            path: path.into(),
            content: content.into_bytes(),
            expected_checksum: expected,
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
}

/// An engine whose every store read publishes `publish(n)` through a second
/// handle, as a background export or another writer would.
fn engine(publish: fn(u32) -> (String, String), flush: bool) -> (TempDir, Engine) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("vault");
    let (vault, _) = Vault::init_with(&root, options(temp.path())).unwrap();
    put(&vault, "alpha.md", "# Alpha\n\npineapple orchard\n".into());
    drop(vault);
    let mut reader = Vault::open_with(&root, options(temp.path())).unwrap();
    let writer = Rc::new(Vault::open_with(&root, options(temp.path())).unwrap());
    let calls = Rc::new(Cell::new(0u32));
    reader.set_hooks(Hooks {
        during_read: Some(Rc::new(move |_| {
            let n = calls.get();
            calls.set(n + 1);
            let (path, content) = publish(n);
            put(&writer, &path, content);
            if flush {
                // Retire the record into git, as the background loop does.
                writer.flush().unwrap();
            }
        })),
        ..Hooks::default()
    });
    (temp, Engine::new(reader, Arc::new(Unavailable)))
}

fn search(engine: &Engine) -> Value {
    engine
        .call("wi_search", json!({"query": "pineapple"}))
        .text_value()
        .unwrap()
}

fn withheld_generation(out: &Value) -> bool {
    out["withheld"]
        .as_array()
        .is_some_and(|w| w.iter().any(|w| w["reason"] == "generation-changed"))
}

#[test]
fn unrelated_publications_during_a_read_do_not_withhold() {
    let (_temp, engine) = engine(
        |n| {
            (
                format!("exports/other-{n}.md"),
                format!("# Other {n}\n\nunrelated\n"),
            )
        },
        false,
    );
    let out = search(&engine);
    assert!(!withheld_generation(&out), "{out}");
    assert_eq!(out["hits"][0]["path"], "alpha.md", "{out}");
    assert!(out["generation"].as_u64().unwrap() >= 3, "{out}");
}

#[test]
fn unrelated_publications_retired_during_a_read_do_not_withhold() {
    let (_temp, engine) = engine(
        |n| (format!("other-{n}.md"), format!("# Other {n}\n")),
        true,
    );
    let out = search(&engine);
    assert!(!withheld_generation(&out), "{out}");
    assert_eq!(out["hits"][0]["path"], "alpha.md", "{out}");
}

#[test]
fn lifecycle_publications_retired_during_a_read_withhold() {
    let (_temp, engine) = engine(
        |n| {
            (
                format!("memory/c-{n}.md"),
                format!("---\ntype: memory\nsummary: c {n}\n---\nbody\n"),
            )
        },
        true,
    );
    let out = search(&engine);
    assert!(withheld_generation(&out), "{out}");
}

#[test]
fn a_change_to_a_returned_hit_never_delivers_the_old_bytes() {
    let (_temp, engine) = engine(
        |n| {
            (
                "alpha.md".into(),
                format!("# Alpha\n\npineapple orchard {n}\n"),
            )
        },
        false,
    );
    let out = search(&engine);
    // The hash check or the generation check withholds it, never neither.
    let reasons: Vec<&str> = out["withheld"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|w| w["reason"].as_str())
        .collect();
    assert!(
        reasons.contains(&"changed-after-index") || reasons.contains(&"generation-changed"),
        "{out}"
    );
    assert_eq!(out["hits"], json!([]), "{out}");
}

#[test]
fn a_lifecycle_note_publication_withholds() {
    let (_temp, engine) = engine(
        |n| {
            (
                format!("memory/claim-{n}.md"),
                format!("---\ntype: memory\nsummary: claim {n}\n---\nbody\n"),
            )
        },
        false,
    );
    let out = search(&engine);
    assert!(withheld_generation(&out), "{out}");
    assert_eq!(out["hits"], json!([]), "{out}");
}
