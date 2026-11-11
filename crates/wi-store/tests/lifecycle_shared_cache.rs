//! Handles of one vault on one index share parsed lifecycle notes. Whatever
//! another handle parsed, and however stale the index is, a handle answers
//! like a fresh handle that reads every lifecycle file.

use std::path::Path;

use tempfile::TempDir;
use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

fn options(temp: &TempDir) -> OpenOptions {
    OpenOptions {
        index_path: Some(temp.path().join("index.db")),
    }
}

fn init(temp: &TempDir) -> Vault {
    Vault::init_with(temp.path().join("vault"), options(temp))
        .unwrap()
        .0
}

fn open(temp: &TempDir, root: &Path) -> Vault {
    Vault::open_with(root, options(temp)).unwrap()
}

fn work(reason: &str) -> String {
    format!(
        "---\ntype: work\nwork_id: w1\nwork_kind: conflict\nwork: {{state: ready, reason: '{reason}'}}\n---\n"
    )
}

fn rule(text: &str) -> String {
    format!(
        "---\ntype: memory\nkind: rule\nclaim_kind: intent\nstatus: active\nsummary: {text}\nentities: []\nscope: personal\napplicability: {{conditions: [], supersedes: []}}\nsources: []\norigin: {{kind: user, locator: file, captured_at: '2026-01-01T00:00:00Z'}}\n---\n{text}\n"
    )
}

fn reasons(vault: &Vault) -> Vec<Option<String>> {
    vault
        .work_list()
        .unwrap()
        .into_iter()
        .map(|w| w.reason)
        .collect()
}

fn summaries(vault: &Vault) -> Vec<Option<String>> {
    vault
        .current_rules()
        .unwrap()
        .rules
        .into_iter()
        .map(|r| r.summary)
        .collect()
}

/// What a fresh handle that ignores materialized snapshots and shares no
/// parsed notes sees.
fn files_only(temp: &TempDir, root: &Path) -> Vault {
    let vault = open(temp, root);
    vault.set_lifecycle_files_only(true);
    vault
}

#[test]
fn a_fresh_handle_sees_an_external_edit_behind_a_stale_index() {
    let temp = TempDir::new().unwrap();
    let warm = init(&temp);
    let path = warm.root().join("work.md");
    std::fs::write(&path, work("before")).unwrap();
    warm.catch_up().unwrap();
    assert_eq!(reasons(&warm), [Some("before".to_owned())]);
    std::fs::write(&path, work("after external edit")).unwrap();
    let reader = open(&temp, warm.root());
    let expected = reasons(&files_only(&temp, warm.root()));
    assert_eq!(expected, [Some("after external edit".to_owned())]);
    assert_eq!(reasons(&reader), expected);
    assert_eq!(reasons(&warm), expected);
}

#[test]
fn bytes_read_behind_a_stale_index_never_answer_for_the_indexed_hash() {
    let temp = TempDir::new().unwrap();
    let owner = init(&temp);
    let path = owner.root().join("work.md");
    std::fs::write(&path, work("canonical")).unwrap();
    owner.catch_up().unwrap();
    std::fs::write(&path, work("temporary external edit")).unwrap();
    let warm = open(&temp, owner.root());
    assert_eq!(reasons(&warm), [Some("temporary external edit".to_owned())]);
    std::fs::write(&path, work("canonical")).unwrap();
    owner.catch_up().unwrap();
    let reader = open(&temp, owner.root());
    let expected = reasons(&files_only(&temp, owner.root()));
    assert_eq!(expected, [Some("canonical".to_owned())]);
    assert_eq!(reasons(&reader), expected);
    assert_eq!(reasons(&warm), expected);
    assert_eq!(reasons(&owner), expected);
}

#[test]
fn a_write_through_one_live_handle_reaches_the_other_at_once() {
    let temp = TempDir::new().unwrap();
    let writer = init(&temp);
    let reader = open(&temp, writer.root());
    let write = |text: &str, expected_checksum: Option<String>| {
        writer
            .write(WriteRequest {
                path: "rule.md".into(),
                content: rule(text).into_bytes(),
                expected_checksum,
                op_id: None,
                ingress: Ingress::Engine,
            })
            .unwrap()
            .checksum
    };
    let first = write("First.", None);
    assert_eq!(summaries(&reader), [Some("First.".to_owned())]);
    write("Second.", Some(first));
    let expected = summaries(&files_only(&temp, writer.root()));
    assert_eq!(expected, [Some("Second.".to_owned())]);
    assert_eq!(summaries(&reader), expected);
    assert_eq!(summaries(&writer), expected);
}

/// The lifecycle data a handle answers with: current rules (judged from
/// fresh bytes) and work entries (taken from the parsed notes).
fn observed(vault: &Vault) -> (Vec<Option<String>>, Vec<Option<String>>) {
    (summaries(vault), reasons(vault))
}

#[test]
fn live_handles_follow_the_files_while_the_index_is_stale_and_after_catch_up() {
    let temp = TempDir::new().unwrap();
    let a = init(&temp);
    let b = open(&temp, a.root());
    let rule_path = a.root().join("rule.md");
    let work_path = a.root().join("work.md");
    std::fs::write(&rule_path, rule("Indexed.")).unwrap();
    std::fs::write(&work_path, work("Indexed.")).unwrap();
    a.catch_up().unwrap();
    assert_eq!(observed(&a), observed(&b));
    for (i, text) in ["Edited once.", "Edited twice.", "Indexed."]
        .into_iter()
        .enumerate()
    {
        // Each edit changes the size, so the stamp differs at any clock
        // resolution.
        let text = format!("{text}{}", "x".repeat(i + 1));
        std::fs::write(&rule_path, rule(&text)).unwrap();
        std::fs::write(&work_path, work(&text)).unwrap();
        let expected = observed(&files_only(&temp, a.root()));
        assert_eq!(expected.1, [Some(text.clone())]);
        assert_eq!(observed(&b), expected, "stale index, edit {i}");
        assert_eq!(observed(&a), expected, "stale index, edit {i}");
        a.catch_up().unwrap();
        let expected = observed(&files_only(&temp, a.root()));
        assert_eq!(observed(&a), expected, "caught up, edit {i}");
        assert_eq!(observed(&b), expected, "caught up, edit {i}");
        let fresh = open(&temp, a.root());
        assert_eq!(observed(&fresh), expected, "fresh handle, edit {i}");
    }
}
