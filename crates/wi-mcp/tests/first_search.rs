//! The first index-ranked MCP call on an index without a complete walk
//! answers from the index after one foreground catch-up, and construction
//! (the `initialize` path) never scans.
mod support;
use support::ToolResultText;

use std::sync::Arc;

use serde_json::{Value, json};
use tempfile::TempDir;
use wi_mcp::{Engine, VaultLifecycle};
use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

/// A vault with one note whose device index has never completed a walk: the
/// vault is initialised against one cache, then opened against a fresh one.
fn fresh_cache_engine() -> (TempDir, Engine) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("vault");
    let (vault, _) = Vault::init_with(
        &root,
        OpenOptions {
            index_path: Some(temp.path().join("first.db")),
        },
    )
    .unwrap();
    vault
        .write(WriteRequest {
            path: "notes/first.md".into(),
            content: b"# First\n\nzebrafish handover\n".to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Engine,
        })
        .unwrap();
    drop(vault);
    let vault = Vault::open_with(
        &root,
        OpenOptions {
            index_path: Some(temp.path().join("second.db")),
        },
    )
    .unwrap();
    vault.initialize_index().unwrap();
    (temp, Engine::new(vault, Arc::new(VaultLifecycle)))
}

fn call(e: &Engine, name: &str, args: Value) -> Value {
    let result = e.call(name, args);
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.text_value().unwrap()
}

#[test]
fn initialize_path_runs_no_catch_up() {
    let (_temp, engine) = fresh_cache_engine();
    assert_eq!(engine.foreground_catch_ups(), 0);
    assert!(engine.vault().index_as_of().unwrap().is_none());
}

#[test]
fn first_search_answers_from_the_index_not_the_fallback() {
    let (_temp, engine) = fresh_cache_engine();
    let out = call(&engine, "wi_search", json!({"query":"zebrafish"}));
    assert_eq!(out["fallback"], false, "{out}");
    assert!(out["index_as_of"].is_string(), "{out}");
    assert!(out["hits"].to_string().contains("notes/first.md"), "{out}");
    assert_eq!(engine.foreground_catch_ups(), 1);
}

#[test]
fn later_calls_skip_a_complete_walk() {
    let (_temp, engine) = fresh_cache_engine();
    call(&engine, "wi_search", json!({"query":"zebrafish"}));
    call(&engine, "wi_search", json!({"query":"zebrafish"}));
    call(&engine, "wi_list", json!({}));
    assert_eq!(engine.foreground_catch_ups(), 1);
}

#[test]
fn reads_do_not_trigger_a_catch_up() {
    let (_temp, engine) = fresh_cache_engine();
    call(&engine, "wi_read", json!({"identifier":"notes/first.md"}));
    assert_eq!(engine.foreground_catch_ups(), 0);
}
