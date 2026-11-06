//! An agent stores memory without hand-made hashes, and its rules and
//! preferences are kept as labelled notes instead of needing the owner.

mod common;

use common::{Fixture, write};
use serde_json::{Value, json};
use wi_store::{CurrentSearch, Ingress, Query, SearchOptions, Vault, WriteRequest, WriteResult};

fn agent_write(vault: &Vault, path: &str, content: &str) -> WriteResult {
    vault
        .lifecycle_write(WriteRequest {
            path: path.into(),
            content: content.as_bytes().to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Mcp,
        })
        .unwrap()
}

fn adoption(result: &WriteResult) -> Value {
    result.lifecycle.as_ref().expect("lifecycle report")["adoption"].clone()
}

fn found(vault: &Vault, text: &str) -> Vec<String> {
    vault
        .search_current(&Query::text(text), &Value::Null, SearchOptions::default())
        .unwrap()
        .response
        .hits
        .into_iter()
        .map(|h| h.rel_path.to_string_lossy().into_owned())
        .collect()
}

fn frontmatter(vault: &Vault, path: &str, key: &str) -> Value {
    let bytes = std::fs::read(vault.root().join(path)).unwrap();
    wi_note::Note::parse(&bytes)
        .unwrap()
        .frontmatter()
        .and_then(|f| f.get(key))
        .unwrap_or(Value::Null)
}

fn tea_note(vault: &Vault) {
    write(
        vault,
        "notes/tea.md",
        "# Tea\n\nThe owner takes oat milk.\n\n## Brewing\n\nThree minutes.\n",
        Ingress::Cli,
    );
}

#[test]
fn an_agent_claim_citing_a_note_path_is_active_and_found_by_default_search() {
    let (_f, vault) = Fixture::new();
    tea_note(&vault);
    let result = agent_write(
        &vault,
        "memory/coffee.md",
        "---\ntype: memory\nsummary: The owner takes oat milk\nsources:\n  - notes/tea.md\n---\nOat milk, always.\n",
    );
    assert_eq!(adoption(&result)["outcome"], "active", "{result:?}");
    // The engine wrote the hash and the classification defaults; a bare
    // file reference pins the whole file and attaches no span.
    let pins = frontmatter(&vault, "memory/coffee.md", "sources");
    let bytes = std::fs::read(vault.root().join("notes/tea.md")).unwrap();
    assert_eq!(pins[0]["ref"], "notes/tea.md");
    assert_eq!(pins[0]["source_hash"], json!(wi_lifecycle::sha256(&bytes)));
    assert!(pins[0].get("locator").is_none(), "{pins}");
    assert!(pins[0].get("span_hash").is_none(), "{pins}");
    assert_eq!(frontmatter(&vault, "memory/coffee.md", "kind"), "fact");
    assert_eq!(
        frontmatter(&vault, "memory/coffee.md", "claim_kind"),
        "historical"
    );
    assert_eq!(frontmatter(&vault, "memory/coffee.md", "scope"), "personal");
    assert!(found(&vault, "oat milk always").contains(&"memory/coffee.md".to_owned()));
}

#[test]
fn a_reference_may_name_a_heading_or_lines_and_a_mapping_without_a_hash() {
    let (_f, vault) = Fixture::new();
    tea_note(&vault);
    // Line numbers count every line of the file, frontmatter included.
    let stored = std::fs::read_to_string(vault.root().join("notes/tea.md")).unwrap();
    let line = 1 + stored
        .lines()
        .position(|l| l == "The owner takes oat milk.")
        .unwrap();
    let result = agent_write(
        &vault,
        "memory/brew.md",
        &format!(
            "---\ntype: memory\nsummary: Brew tea for three minutes\nsources:\n  - ref: notes/tea#Brewing\n  - notes/tea.md#L{line}\n---\nThree minutes.\n"
        ),
    );
    assert_eq!(adoption(&result)["outcome"], "active", "{result:?}");
    let bytes = std::fs::read(vault.root().join("notes/tea.md")).unwrap();
    let pins = frontmatter(&vault, "memory/brew.md", "sources");
    let text = |pin: &Value| {
        let span = &pin["locator"]["span"];
        let (start, end) = (
            span["start"].as_u64().unwrap() as usize,
            span["end"].as_u64().unwrap() as usize,
        );
        String::from_utf8(bytes[start..end].to_vec()).unwrap()
    };
    assert_eq!(pins[0]["ref"], "notes/tea.md");
    assert_eq!(text(&pins[0]), "## Brewing\n\nThree minutes.\n");
    assert_eq!(text(&pins[1]), "The owner takes oat milk.\n");
}

#[test]
fn an_agent_claim_citing_a_missing_path_stays_a_candidate_with_a_clear_reason() {
    let (_f, vault) = Fixture::new();
    let result = agent_write(
        &vault,
        "memory/ghost.md",
        "---\ntype: memory\nsummary: Ghost fact\nsources:\n  - ref: notes/absent.md\n---\nGhost.\n",
    );
    let adoption = adoption(&result);
    assert_eq!(adoption["outcome"], "candidate", "{result:?}");
    let gate = adoption["gates"]["sources"].as_str().unwrap();
    assert!(gate.contains("missing notes/absent.md"), "{gate}");
    // Classification was completed; only the source is missing.
    assert_eq!(adoption["gates"]["classification"], "pass", "{adoption}");
    let hits = found(&vault, "Ghost");
    assert!(!hits.contains(&"memory/ghost.md".to_owned()), "{hits:?}");
}

#[test]
fn an_agent_preference_is_noted_searchable_and_not_protected() {
    let (_f, vault) = Fixture::new();
    let result = agent_write(
        &vault,
        "memory/pref-korean.md",
        "---\ntype: memory\nkind: preference\nsummary: Answer in Korean\n---\nAnswer in Korean.\n",
    );
    let adoption = adoption(&result);
    assert_eq!(adoption["outcome"], "noted", "{result:?}");
    assert_eq!(adoption["path"], "preferences/pref-korean.md");
    assert_eq!(result.path, "preferences/pref-korean.md");
    assert_eq!(frontmatter(&vault, &result.path, "noted_by"), "agent");
    assert_eq!(
        frontmatter(&vault, &result.path, "title"),
        "Answer in Korean"
    );
    assert!(frontmatter(&vault, &result.path, "noted_at").is_string());
    assert!(!vault.root().join("memory/pref-korean.md").exists());
    assert_eq!(found(&vault, "Korean"), ["preferences/pref-korean.md"]);
    // The protected list is unchanged and no owner work item exists.
    assert!(vault.current_rules().unwrap().rules.is_empty());
    assert!(vault.work_list().unwrap().is_empty());
    let noted = vault.noted_notes().unwrap();
    assert_eq!(noted.len(), 1);
    assert_eq!(noted[0].kind, "preference");
    assert_eq!(noted[0].content, "Answer in Korean.");
    // A second note with the same name does not overwrite the first.
    let again = agent_write(
        &vault,
        "memory/pref-korean.md",
        "---\ntype: memory\nkind: preference\n---\nAnswer briefly.\n",
    );
    assert_eq!(again.path, "preferences/pref-korean-2.md");
}

#[test]
fn a_user_preference_is_still_protected() {
    let (_f, vault) = Fixture::new();
    let result = vault
        .lifecycle_write(WriteRequest {
            path: "memory/lang.md".into(),
            content: b"---\ntype: memory\nkind: preference\n---\nAnswer in Korean.\n".to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    assert_eq!(adoption(&result)["outcome"], "active", "{result:?}");
    assert_eq!(vault.current_rules().unwrap().rules.len(), 1);
    assert!(vault.noted_notes().unwrap().is_empty());
}

fn search(vault: &Vault, text: &str) -> CurrentSearch {
    vault
        .search_current(&Query::text(text), &Value::Null, SearchOptions::default())
        .unwrap()
}

/// A body of `lines` distinct lines, about 50 bytes each.
fn large_body(lines: usize) -> String {
    (0..lines)
        .map(|i| format!("Line {i:05} of a long reference note about kettles.\n"))
        .collect()
}

#[test]
fn a_short_claim_citing_a_large_note_is_delivered_within_bounds() {
    let (_f, vault) = Fixture::new();
    let body = format!("# Kettle\n\n## Detail\n\n{}", large_body(1200));
    assert!(body.len() > 50 * 1024);
    write(&vault, "notes/kettle.md", &body, Ingress::Cli);
    let result = agent_write(
        &vault,
        "memory/kettle.md",
        "---\ntype: memory\nsummary: The kettle boils fast\nsources:\n  - notes/kettle.md\n  - notes/kettle.md#Detail\n---\nThe kettle boils fast.\n",
    );
    assert_eq!(adoption(&result)["outcome"], "active", "{result:?}");
    let pins = frontmatter(&vault, "memory/kettle.md", "sources");
    assert!(pins[0].get("locator").is_none(), "{pins}");
    // The heading section is cut on a line boundary at the cap and marked.
    let span = &pins[1]["locator"]["span"];
    assert_eq!(span["cut"], true, "{pins}");
    let bytes = std::fs::read(vault.root().join("notes/kettle.md")).unwrap();
    let (start, end) = (
        span["start"].as_u64().unwrap() as usize,
        span["end"].as_u64().unwrap() as usize,
    );
    assert!(end - start <= wi_store::SPAN_MAX_BYTES);
    let text = std::str::from_utf8(&bytes[start..end]).unwrap();
    assert!(
        text.starts_with("## Detail\n") && text.ends_with('\n'),
        "{text:?}"
    );
    // A current read delivers the claim with a bounded attachment.
    let found = search(&vault, "kettle boils fast");
    let at = found
        .response
        .hits
        .iter()
        .position(|h| h.rel_path.to_string_lossy() == "memory/kettle.md")
        .unwrap_or_else(|| panic!("claim withheld: {:?}", found.response.withheld));
    assert!(
        found.response.withheld.is_empty(),
        "{:?}",
        found.response.withheld
    );
    let label = &found.labels[at];
    assert_eq!(label.eligibility, "current");
    let attached: usize = label.spans.iter().map(|s| s.text.len()).sum();
    assert!(attached <= wi_store::SPAN_MAX_BYTES, "{attached}");
}

#[test]
fn agent_and_user_claims_get_the_same_classification_and_read_by_summary() {
    let (_f, vault) = Fixture::new();
    tea_note(&vault);
    agent_write(
        &vault,
        "memory/coffee-fact.md",
        "---\ntype: memory\nkind: fact\nsummary: The owner drinks coffee at nine\nsources:\n  - notes/tea.md\n---\nCoffee at nine.\n",
    );
    vault
        .lifecycle_write(WriteRequest {
            path: "memory/user-fact.md".into(),
            content: b"---\ntype: memory\nkind: fact\n---\nCoffee at ten.\n".to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    assert_eq!(
        frontmatter(&vault, "memory/coffee-fact.md", "claim_kind"),
        frontmatter(&vault, "memory/user-fact.md", "claim_kind"),
    );
    let found = search(&vault, "coffee nine");
    let hit = found
        .response
        .hits
        .iter()
        .find(|h| h.rel_path.to_string_lossy() == "memory/coffee-fact.md")
        .expect("agent claim found");
    assert_eq!(hit.title, "The owner drinks coffee at nine");
}
