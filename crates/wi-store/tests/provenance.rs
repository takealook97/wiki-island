//! S3: provenance is carried by the note itself. File ingress (editor,
//! Obsidian, file sync, git merge) never relabels an agent note as the
//! owner's; removing the agent label (or `wi adopt`) is owner adoption.

mod common;

use common::*;
use wi_store::{Ingress, Vault};

const AGENT_FACT: &str = "---\ntype: memory\nkind: fact\nsummary: agent fact\norigin:\n  kind: agent\n  locator: mcp:write\nstatus: active\n---\nbody\n";

fn text(vault: &Vault, rel: &str) -> String {
    std::fs::read_to_string(vault.root().join(rel)).unwrap()
}

fn seed_agent(vault: &Vault, rel: &str, content: &str) {
    write(vault, rel, content, Ingress::Engine);
    vault.flush().unwrap();
}

#[test]
fn agent_note_edited_by_file_ingress_stays_agent() {
    let (_fx, vault) = Fixture::new();
    seed_agent(&vault, "m.md", AGENT_FACT);
    std::fs::write(
        vault.root().join("m.md"),
        AGENT_FACT.replace("summary: agent fact", "summary: edited in an editor"),
    )
    .unwrap();
    vault.ingest_path("m.md").unwrap();
    let now = text(&vault, "m.md");
    assert!(now.contains("kind: agent"), "{now}");
    assert!(!now.contains("kind: user"), "{now}");
    // The claim changed, so the note goes back through the gates.
    assert!(now.contains("status: candidate"), "{now}");
}

#[test]
fn synced_in_agent_note_stays_agent() {
    let (_fx, vault) = Fixture::new();
    // A note another device's engine wrote arrives as a new file.
    std::fs::write(vault.root().join("synced.md"), AGENT_FACT).unwrap();
    vault.ingest_path("synced.md").unwrap();
    let now = text(&vault, "synced.md");
    assert!(now.contains("kind: agent"), "{now}");
    assert!(!now.contains("kind: user"), "{now}");
}

#[test]
fn removing_the_agent_label_in_an_editor_adopts_the_note() {
    let (_fx, vault) = Fixture::new();
    seed_agent(&vault, "m.md", AGENT_FACT);
    std::fs::write(
        vault.root().join("m.md"),
        AGENT_FACT.replace("origin:\n  kind: agent\n  locator: mcp:write\n", ""),
    )
    .unwrap();
    vault.ingest_path("m.md").unwrap();
    let now = text(&vault, "m.md");
    assert!(now.contains("kind: user"), "{now}");
    assert!(now.contains("locator: file"), "{now}");
}

#[test]
fn user_note_is_never_downgraded_by_file_ingress() {
    let (_fx, vault) = Fixture::new();
    let user = AGENT_FACT.replace(
        "kind: agent\n  locator: mcp:write",
        "kind: user\n  locator: file",
    );
    seed_agent(&vault, "u.md", &user);
    std::fs::write(
        vault.root().join("u.md"),
        user.replace("summary: agent fact", "summary: changed"),
    )
    .unwrap();
    vault.ingest_path("u.md").unwrap();
    assert!(text(&vault, "u.md").contains("kind: user"));
}

#[test]
fn take_ownership_relabels_agent_notes_and_noted_rules_only() {
    let (_fx, vault) = Fixture::new();
    seed_agent(&vault, "m.md", AGENT_FACT);
    let result = vault.take_ownership("m.md").unwrap();
    assert!(result.is_some());
    let now = text(&vault, "m.md");
    assert!(
        now.contains("kind: user") && now.contains("cli:adopt"),
        "{now}"
    );
    // A second call has nothing to do.
    assert!(vault.take_ownership("m.md").unwrap().is_none());

    seed_agent(
        &vault,
        "rules/tabs.md",
        "---\ntitle: tabs\nnoted_by: agent\nnoted_at: 2026-11-05T12:00:00Z\n---\n\nUse tabs.\n",
    );
    assert!(vault.take_ownership("rules/tabs.md").unwrap().is_some());
    assert!(!text(&vault, "rules/tabs.md").contains("noted_by"));
}

#[test]
fn rules_export_takes_only_user_origin_after_file_edits() {
    let (_fx, vault) = Fixture::new();
    let agent_rule = AGENT_FACT.replace("kind: fact", "kind: rule");
    seed_agent(&vault, "r.md", &agent_rule);
    std::fs::write(
        vault.root().join("r.md"),
        agent_rule.replace("summary: agent fact", "summary: edited"),
    )
    .unwrap();
    vault.ingest_path("r.md").unwrap();
    vault.flush().unwrap();
    let set = vault.current_rules().unwrap();
    assert!(
        set.rules.iter().all(|r| r.path != "r.md"),
        "an agent rule edited by file ingress is not a user rule"
    );
}
