mod common;
use common::{Fixture, write};
use serde_json::{Value, json};
use std::{fs, path::Path, rc::Rc};
use wi_store::{Hooks, Ingress, MoveRequest, Query, SearchOptions, WriteRequest};

#[test]
fn automatic_line_span_stops_on_a_multibyte_boundary() {
    let (_fixture, vault) = Fixture::new();
    let long_line = format!("{}{}\n", "x".repeat(1023), "한".repeat(20));
    write(&vault, "source.md", &long_line, Ingress::Cli);
    let source = fs::read_to_string(vault.root().join("source.md")).unwrap();
    let line = source.lines().position(|s| s.starts_with("xxx")).unwrap() + 1;
    let result = vault.lifecycle_write(WriteRequest {
        path: "memory/boundary.md".into(),
        content: format!("---\ntype: memory\nsummary: Boundary evidence\nsources: [source.md#L{line}]\n---\nBoundary evidence.\n").into_bytes(),
        expected_checksum: None, op_id: None, ingress: Ingress::Mcp,
    }).unwrap();
    assert_eq!(
        result.lifecycle.as_ref().unwrap()["adoption"]["outcome"],
        "active"
    );
    let memory = vault.read("memory/boundary.md").unwrap();
    let note = wi_note::Note::parse(&memory.bytes).unwrap();
    let pins = note.frontmatter().unwrap().get("sources").unwrap();
    let span = &pins[0]["locator"]["span"];
    let start = span["start"].as_u64().unwrap() as usize;
    let end = span["end"].as_u64().unwrap() as usize;
    println!("span={span}; bytes={}", end - start);
    assert_eq!(end - start, 1023);
    assert_eq!(span["cut"], true);
    assert!(source.is_char_boundary(start) && source.is_char_boundary(end));
    let found = vault
        .search_current(
            &Query::text("Boundary evidence"),
            &Value::Null,
            SearchOptions::default(),
        )
        .unwrap();
    assert!(
        found
            .response
            .hits
            .iter()
            .any(|h| h.rel_path.to_string_lossy() == "memory/boundary.md")
    );
    assert!(found.response.withheld.is_empty());
}

#[test]
fn an_out_of_range_line_locator_stays_a_candidate() {
    let (_fixture, vault) = Fixture::new();
    write(&vault, "source.md", "Small reference.\n", Ingress::Cli);
    let result = vault.lifecycle_write(WriteRequest {
        path: "memory/past-end.md".into(),
        content: b"---\ntype: memory\nsummary: Beyond range\nsources: [source.md#L999999]\n---\nBeyond range.\n".to_vec(),
        expected_checksum: None, op_id: None, ingress: Ingress::Mcp,
    }).unwrap();
    println!("adoption={}", result.lifecycle.as_ref().unwrap());
    assert_eq!(
        result.lifecycle.as_ref().unwrap()["adoption"]["outcome"],
        "candidate"
    );
    let found = vault
        .search_current(
            &Query::text("Beyond range"),
            &json!(null),
            SearchOptions::default(),
        )
        .unwrap();
    assert!(
        !found
            .response
            .hits
            .iter()
            .any(|h| h.rel_path.to_string_lossy() == "memory/past-end.md")
    );
}

#[test]
fn deleting_the_move_source_after_its_check_returns_stale() {
    let (_fixture, mut vault) = Fixture::new();
    write(&vault, "owner.md", "Owner bytes.\n", Ingress::Cli);
    vault.flush().unwrap();
    let expected = vault.read("owner.md").unwrap().checksum;
    let source = vault.root().join("owner.md");
    vault.set_hooks(Hooks {
        before_swap: Some(Rc::new(move |_target: &Path| {
            fs::remove_file(&source).unwrap();
        })),
        ..Hooks::default()
    });
    let result = vault.move_note(MoveRequest {
        from: "owner.md".into(),
        to: "moved.md".into(),
        expected_checksum: Some(expected),
        op_id: None,
        ingress: Ingress::Mcp,
    });
    vault.set_hooks(Hooks::default());
    println!(
        "move_result={result:?}; source_exists={}; target_exists={}",
        vault.root().join("owner.md").exists(),
        vault.root().join("moved.md").exists()
    );
    assert!(
        matches!(result, Err(wi_store::StoreError::Stale { .. })),
        "an owner deletion must stop the move: {result:?}"
    );
    assert!(!vault.root().join("moved.md").exists());
}

#[cfg(unix)]
#[test]
fn restored_permissions_close_the_recheck_after_automatic_ingest() {
    use std::os::unix::fs::PermissionsExt;
    let (_fixture, vault) = Fixture::new();
    vault
        .maintain(&wi_store::MaintainOptions::default())
        .unwrap();
    let path = vault.root().join("secret.md");
    fs::write(&path, "Owner reference.\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        fs::read(&path).is_err(),
        "permission test must really deny reads"
    );
    vault.auto_ingest().unwrap();
    vault.flush().unwrap();
    vault
        .maintain(&wi_store::MaintainOptions::default())
        .unwrap();
    let ready = |v: &wi_store::Vault| {
        v.work_list()
            .unwrap()
            .into_iter()
            .filter(|w| {
                w.target.as_deref() == Some("secret.md") && w.state.as_deref() != Some("closed")
            })
            .collect::<Vec<_>>()
    };
    let before = ready(&vault);
    println!("unreadable_items={before:?}");
    assert_eq!(before.len(), 1);
    let reason = before[0].reason.as_ref().unwrap();
    assert!(!reason.contains("os error") && !reason.contains(vault.root().to_str().unwrap()));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    vault.auto_ingest().unwrap();
    vault.flush().unwrap();
    let report = vault
        .maintain(&wi_store::MaintainOptions::default())
        .unwrap();
    println!("healed_report={report:?}; remaining={:?}", ready(&vault));
    assert!(ready(&vault).is_empty());
    assert!(report.work_closed > 0);
    vault
        .maintain(&wi_store::MaintainOptions::default())
        .unwrap();
    assert!(ready(&vault).is_empty());
}
