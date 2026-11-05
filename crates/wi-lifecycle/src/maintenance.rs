//! The built-in maintenance procedure. Its text is compiled into the
//! binary; the vault's maintenance pass materializes it as an engine-origin
//! procedure note and rewrites it whenever the binary's text differs, so
//! every host agent reads the same upkeep instructions.

/// `skill_id` of the built-in maintenance procedure.
pub const SKILL_ID: &str = "wiki-island.maintenance";

/// Revision of the compiled procedure text.
pub const VERSION: u32 = 2;

/// One-line summary (procedure `summary` and host skill description).
pub const SUMMARY: &str = "Keep this wiki-island vault healthy: work through open maintenance items within a small budget, and save durable memory as claims.";

/// The procedure body (Markdown, no frontmatter).
pub const PROCEDURE_MARKDOWN: &str = r#"# Vault maintenance

wiki-island repairs its own files and index. Some upkeep needs judgment; it
waits as work items. Use this procedure when `wi_work` reports open items,
or at the end of a session.

## Work through open items

1. Call `wi_work` with `action: "next"`. It returns at most 3 items and a
   byte budget. Do nothing when it returns no items.
2. For each item, read the notes it names, then fix the cause with MCP
   writes (`wi_write`, `wi_edit`, `wi_move`) and the checksum you read.
   A write without a matching checksum is refused; read again and retry once.
3. Close the item through `wi_work` with an honest outcome. Use
   `not_supported` or `evidence_unavailable` with limits and a reopen
   trigger when you cannot fix it.
4. Stop when the budget is spent or after 3 items, even if more remain.
   The next session continues.

## Improve procedures

- When you repeat the same workflow 3 or more times, write it as a new
  procedure note (`type: memory`, `kind: procedure`, a unique `skill_id`).
- When an item says a procedure is stale, too large, duplicated or failing
  lint, revise that procedure as a candidate revision; do not edit it in place.

## Never

- Never create, change or remove rules or preferences (`kind: rule`,
  `kind: preference`). Only the owner writes those.
- Never change files under `.wiki-island/` directly.
- Never ask the owner to run a command. If something needs the owner, say
  so in one plain sentence.

## How to write memory

Write a claim when you learn something durable that should outlive the
session: a fact about the owner's work, a decision and its reason, or a
procedure. Store nothing when the session produced nothing durable.
Capture through `wi_capture` or a new note file only. Never rewrite rules
or preferences while capturing. Do not save facts that are cheap to
re-query, one-off statuses, secrets or chat transcripts.

A claim is a Markdown note with this minimal frontmatter:

```yaml
---
type: memory
kind: fact          # fact, decision, procedure or event
summary: One sentence that states the claim.
---
The supporting detail, in plain words.
```

Add `sources` that pin the notes or files the claim rests on when you have
them. The engine records who wrote the claim and when, adopts it once its
evidence checks pass, and lists what is missing as a work item.
"#;

/// The procedure note exactly as the engine writes it.
pub fn note_bytes() -> String {
    format!(
        "---\ntitle: {title}\ntype: memory\nkind: procedure\nskill_id: {SKILL_ID}\nmaintenance_version: {VERSION}\nstatus: active\nsummary: {summary}\norigin:\n  kind: engine\napplicability:\n  conditions: []\nsources: []\n---\n\n{PROCEDURE_MARKDOWN}",
        title = json_scalar("Vault maintenance"),
        summary = json_scalar(SUMMARY),
    )
}

/// A JSON string, which YAML 1.1 and 1.2 loaders both read as a scalar.
fn json_scalar(text: &str) -> String {
    serde_json::Value::String(text.into()).to_string()
}

/// File name of the materialized procedure inside the procedures folder.
pub fn file_name() -> String {
    format!("{SKILL_ID}.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_procedure_parses_as_a_current_engine_procedure() {
        let note = wi_note::Note::parse(note_bytes().as_bytes()).unwrap();
        let lifecycle = note.lifecycle();
        assert_eq!(lifecycle.lifecycle_type, wi_note::LifecycleType::Memory);
        assert_eq!(lifecycle.kind, Some(wi_note::Kind::Procedure));
        assert_eq!(lifecycle.skill_id.as_deref(), Some(SKILL_ID));
        assert_eq!(
            lifecycle.origin.as_ref().map(|o| o.kind.clone()),
            Some(wi_note::OriginKind::Engine)
        );
        assert!(note.diagnostics().is_empty(), "{:?}", note.diagnostics());
        assert!(note_bytes().len() < 16_384);
        assert!(PROCEDURE_MARKDOWN.contains("type: memory"));
        assert!(PROCEDURE_MARKDOWN.contains("at most 3 items"));
    }
}
