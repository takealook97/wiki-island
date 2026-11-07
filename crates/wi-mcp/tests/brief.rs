//! The standing brief enumerates every eligible pinned note before fitting.

use tempfile::TempDir;
use wi_mcp::{BRIEF_MAX_BYTES, brief};
use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

fn vault() -> (TempDir, Vault) {
    let temp = TempDir::new().unwrap();
    let (vault, _) = Vault::init_with(
        temp.path().join("vault"),
        OpenOptions {
            index_path: Some(temp.path().join("index.db")),
        },
    )
    .unwrap();
    (temp, vault)
}

fn put(vault: &Vault, path: &str, content: &str) {
    vault
        .write(WriteRequest {
            path: path.into(),
            content: content.as_bytes().to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Engine,
        })
        .unwrap();
}

#[test]
fn every_pinned_note_is_considered_newest_first_and_counted() {
    let (_temp, vault) = vault();
    for i in 0..70 {
        // The six newest notes sort after the first 64 by path and rank.
        let (summary, observed) = if i < 64 {
            ("x".repeat(300), "2026-01-01T00:00:00Z".to_owned())
        } else {
            (
                format!("Newest {i}"),
                format!("2026-11-{:02}T00:00:00Z", i - 63),
            )
        };
        put(
            &vault,
            &format!("p{i:02}.md"),
            &format!(
                "---\npinned: true\nsummary: {summary}\nobserved_at: \"{observed}\"\n---\nPinned body.\n"
            ),
        );
    }
    let out = brief(&vault).unwrap();
    assert!(serde_json::to_vec(&out).unwrap().len() <= BRIEF_MAX_BYTES);
    let paths: Vec<&str> = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["path"].as_str())
        .collect();
    assert_eq!(
        &paths[..6],
        ["p69.md", "p68.md", "p67.md", "p66.md", "p65.md", "p64.md"],
        "{out}"
    );
    let omitted = out["omitted"].as_u64().unwrap() as usize;
    assert_eq!(paths.len() + omitted, 70, "{out}");
    assert!(
        out["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["kind"] == "pinned")
    );
}

#[test]
fn ai_noted_preferences_have_a_labelled_section_after_protected_items() {
    let (_temp, vault) = vault();
    put(
        &vault,
        "pinned.md",
        "---\npinned: true\nsummary: Pinned guidance\n---\nPinned guidance.",
    );
    put(
        &vault,
        "noted.md",
        "---\nnoted_by: agent\nsummary: Answer in Korean\n---\nAnswer in Korean.",
    );
    let out = brief(&vault).unwrap();
    assert_eq!(out["items"][0]["path"], "pinned.md");
    assert_eq!(out["items"].as_array().unwrap().len(), 1);
    assert_eq!(out["noted_by_ai"]["label"], "Noted by your AI");
    assert_eq!(out["noted_by_ai"]["items"][0]["path"], "noted.md");
    assert_eq!(out["noted_by_ai"]["items"][0]["noted_by"], "agent");
    assert!(serde_json::to_vec(&out).unwrap().len() <= BRIEF_MAX_BYTES);
}
