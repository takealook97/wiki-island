use serde_json::{Value, json};
use std::{
    fs,
    io::{Cursor, Write},
};
use tempfile::TempDir;
use wi_docs::{ImportOptions, ImportReport, Status, import};
use wi_note::Note;
use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

fn setup() -> (TempDir, Vault) {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, _) = Vault::init_with(
        tmp.path().join("vault"),
        OpenOptions {
            index_path: Some(tmp.path().join("cache.sqlite3")),
        },
    )
    .unwrap();
    (tmp, vault)
}

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn put(vault: &Vault, path: &str, bytes: impl AsRef<[u8]>) {
    fs::create_dir_all(vault.root().join(path).parent().unwrap()).unwrap();
    fs::write(vault.root().join(path), bytes).unwrap();
}

fn read(vault: &Vault, path: &str) -> Note {
    Note::parse(&vault.read(path).unwrap().bytes).unwrap()
}

fn field(note: &Note, key: &str) -> Value {
    note.frontmatter().unwrap().get(key).unwrap()
}

fn run(vault: &Vault, path: &str) -> ImportReport {
    import(vault, path, ImportOptions::default()).unwrap()
}

fn counts(report: &ImportReport) -> (usize, usize, usize, usize, usize) {
    let c = report.conversations.as_ref().expect("conversation counts");
    (c.created, c.updated, c.unchanged, c.conflicts, c.skipped)
}

fn bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

fn claude_conversations() -> Value {
    json!([
        {"uuid": "11111111-aaaa", "name": "Trip plan", "created_at": "2025-03-04T05:06:07Z",
         "updated_at": "2025-03-04T06:00:00Z",
         "chat_messages": [
            {"sender": "human", "text": "Where should we go?",
             "content": [{"type": "text", "text": "Where should we go?"}],
             "attachments": [{"file_name": "map.pdf"}], "files": []},
            {"sender": "assistant", "text": "Lisbon.",
             "content": [{"type": "thinking", "text": "hmm"}, {"type": "text", "text": "Lisbon."}]},
            {"sender": "human", "text": "Thanks"}
         ]},
        {"uuid": "22222222-bbbb", "name": "Empty chat", "created_at": "2025-03-05T00:00:00Z", "chat_messages": []},
        "not a conversation",
        {"uuid": "33333333-cccc", "name": "Notes/2025: draft?", "created_at": "2025-04-01T10:00:00Z",
         "chat_messages": [{"sender": "human", "text": "Second chat"}]}
    ])
}

fn claude_projects() -> Value {
    json!([{"uuid": "p1", "name": "Alpha", "description": "d",
            "docs": [{"uuid": "d1", "filename": "notes.txt", "content": "Project notes\n\nline two\n"},
                     {"uuid": "d2", "filename": "blank.md", "content": ""}]}])
}

fn claude_zip() -> Vec<u8> {
    zip(&[
        ("conversations.json", &bytes(&claude_conversations())),
        ("projects.json", &bytes(&claude_projects())),
        ("users.json", b"[]"),
        ("images/not [read] #1.png", b"\x89PNG"),
    ])
}

#[test]
fn claude_export_imports_conversations() {
    let (_tmp, vault) = setup();
    put(&vault, "claude.zip", claude_zip());
    let report = run(&vault, "claude.zip");
    assert_eq!(report.status, Status::Complete);
    assert_eq!(report.sidecar_path, "AI/Claude");
    // Two conversations and one project document; the empty chat, the stray
    // string and the blank document are skipped.
    assert_eq!(counts(&report), (3, 0, 0, 0, 3));

    let note = read(&vault, "AI/Claude/2025-03-04 Trip plan.md");
    assert_eq!(field(&note, "type"), "note");
    assert_eq!(field(&note, "title"), "Trip plan");
    assert_eq!(field(&note, "conversation_source"), "claude");
    assert_eq!(field(&note, "conversation_id"), "11111111-aaaa");
    assert_eq!(field(&note, "conversation_created"), "2025-03-04T05:06:07Z");
    assert_eq!(field(&note, "conversation_updated"), "2025-03-04T06:00:00Z");
    assert!(
        field(&note, "conversation_checksum")
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(
        note.body(),
        "## User\n\nWhere should we go?\n\nAttachment: map.pdf\n\n## Assistant\n\nLisbon.\n\n## User\n\nThanks\n"
    );
    assert!(!note.body().contains("hmm"));

    let second = read(&vault, "AI/Claude/2025-04-01 Notes 2025 draft.md");
    assert_eq!(second.body(), "## User\n\nSecond chat\n");

    let doc = read(&vault, "AI/Claude/Projects/Alpha/notes.txt.md");
    assert_eq!(doc.body(), "Project notes\n\nline two\n");
    assert_eq!(field(&doc, "claude_project"), "Alpha");
    assert_eq!(field(&doc, "type"), "note");
}

fn gpt_message(role: &str, text: &str, time: f64) -> Value {
    json!({"id": "m", "author": {"role": role}, "create_time": time,
           "content": {"content_type": "text", "parts": [text]}, "metadata": {}, "recipient": "all"})
}

fn gpt_node(parent: Option<&str>, children: &[&str], message: Value) -> Value {
    json!({"id": "n", "parent": parent, "children": children, "message": message})
}

fn chatgpt_conversations() -> Value {
    let mut hidden = gpt_message("assistant", "hidden reasoning", 5.0);
    hidden["metadata"] = json!({"is_visually_hidden_from_conversation": true});
    json!([{
        "id": "g-1", "conversation_id": "g-1", "title": "Rust help",
        "create_time": 1_741_050_367.25, "update_time": 1_741_050_999.0,
        "current_node": "a2",
        "mapping": {
            "root": gpt_node(None, &["sys"], Value::Null),
            "sys": gpt_node(Some("root"), &["u1"], gpt_message("system", "System prompt", 1.0)),
            "u1": gpt_node(Some("sys"), &["a1", "a2"], gpt_message("user", "How do I borrow?", 2.0)),
            "a1": gpt_node(Some("u1"), &[], gpt_message("assistant", "Discarded branch", 3.0)),
            "a2": gpt_node(Some("u1"), &["tool"], gpt_message("assistant", "Use a reference.", 4.0)),
            "tool": gpt_node(Some("a2"), &["h"], gpt_message("tool", "tool output", 4.5)),
            "h": gpt_node(Some("tool"), &[], hidden)
        }
    }])
}

#[test]
fn chatgpt_export_imports_conversations() {
    let (_tmp, vault) = setup();
    // The conversation file sits one directory below the archive root.
    put(
        &vault,
        "chatgpt.zip",
        zip(&[
            (
                "export/conversations.json",
                &bytes(&chatgpt_conversations()),
            ),
            ("export/chat.html", b"<html></html>"),
            ("export/file-abc-image.png", b"\x89PNG"),
        ]),
    );
    let report = run(&vault, "chatgpt.zip");
    assert_eq!(report.sidecar_path, "AI/ChatGPT");
    assert_eq!(counts(&report), (1, 0, 0, 0, 0));
    let note = read(&vault, "AI/ChatGPT/2025-03-04 Rust help.md");
    assert_eq!(field(&note, "type"), "note");
    assert_eq!(field(&note, "conversation_source"), "chatgpt");
    assert_eq!(field(&note, "conversation_id"), "g-1");
    assert_eq!(
        note.body(),
        "## User\n\nHow do I borrow?\n\n## Assistant\n\nUse a reference.\n"
    );
    for gone in ["System prompt", "Discarded", "tool output", "hidden"] {
        assert!(!note.body().contains(gone), "{gone}");
    }
}

#[test]
fn folder_and_single_json_imports_work() {
    let (_tmp, vault) = setup();
    put(
        &vault,
        "unpacked/conversations.json",
        bytes(&claude_conversations()),
    );
    put(&vault, "unpacked/projects.json", bytes(&claude_projects()));
    let report = run(&vault, "unpacked");
    assert_eq!(counts(&report), (3, 0, 0, 0, 3));

    let (_tmp2, other) = setup();
    put(&other, "one.json", bytes(&chatgpt_conversations()));
    let report = run(&other, "one.json");
    assert_eq!(counts(&report), (1, 0, 0, 0, 0));
    assert!(other.read("AI/ChatGPT/2025-03-04 Rust help.md").is_ok());

    // Any other JSON file is still refused as an unsupported document.
    put(&other, "other.json", b"{\"a\": 1}");
    assert!(import(&other, "other.json", ImportOptions::default()).is_err());
    put(&other, "list.json", b"[1, 2]");
    assert!(import(&other, "list.json", ImportOptions::default()).is_err());
}

fn modified(vault: &Vault, path: &str) -> std::time::SystemTime {
    fs::metadata(vault.root().join(path))
        .unwrap()
        .modified()
        .unwrap()
}

#[test]
fn conversation_reimport_is_unchanged() {
    let (_tmp, vault) = setup();
    put(&vault, "claude.zip", claude_zip());
    let first = run(&vault, "claude.zip");
    assert!(!first.unchanged);
    let note = "AI/Claude/2025-03-04 Trip plan.md";
    let manifest = ".wiki-island/imports/conversations/claude.md";
    let before = (
        vault.read(note).unwrap().checksum,
        modified(&vault, note),
        modified(&vault, manifest),
    );
    std::thread::sleep(std::time::Duration::from_millis(30));
    let second = run(&vault, "claude.zip");
    assert!(second.unchanged);
    assert_eq!(
        second.conversations.as_ref().unwrap().conflict_paths,
        Vec::<String>::new()
    );
    assert_eq!(counts(&second), (0, 0, 3, 0, 3));
    let after = (
        vault.read(note).unwrap().checksum,
        modified(&vault, note),
        modified(&vault, manifest),
    );
    assert_eq!(before, after);
}

#[test]
fn a_changed_source_updates_an_unedited_note() {
    let (_tmp, vault) = setup();
    put(&vault, "claude.zip", claude_zip());
    run(&vault, "claude.zip");
    let mut conversations = claude_conversations();
    conversations[0]["chat_messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"sender": "assistant", "text": "Enjoy the trip."}));
    conversations[0]["name"] = json!("Trip plan (renamed)");
    put(
        &vault,
        "claude.zip",
        zip(&[("conversations.json", &bytes(&conversations))]),
    );
    let report = run(&vault, "claude.zip");
    assert_eq!(counts(&report), (0, 1, 1, 0, 2));
    // The path is stable; the title follows the source.
    let note = read(&vault, "AI/Claude/2025-03-04 Trip plan.md");
    assert!(note.body().ends_with("## Assistant\n\nEnjoy the trip.\n"));
    assert_eq!(field(&note, "title"), "Trip plan (renamed)");
}

#[test]
fn conversation_edited_note_reports_conflict() {
    let (_tmp, vault) = setup();
    put(&vault, "claude.zip", claude_zip());
    run(&vault, "claude.zip");
    let path = "AI/Claude/2025-03-04 Trip plan.md";
    let before = vault.read(path).unwrap();
    let mut edited = before.bytes.clone();
    edited.extend_from_slice(b"\nMy own remark.\n");
    vault
        .write(WriteRequest {
            path: path.into(),
            content: edited.clone(),
            expected_checksum: Some(before.checksum),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    let mut conversations = claude_conversations();
    conversations[0]["chat_messages"][2]["text"] = json!("Thanks a lot");
    put(
        &vault,
        "claude.zip",
        zip(&[("conversations.json", &bytes(&conversations))]),
    );
    let report = run(&vault, "claude.zip");
    assert_eq!(report.status, Status::Partial);
    let c = report.conversations.as_ref().unwrap();
    assert_eq!(c.conflicts, 1);
    assert_eq!(c.conflict_paths, [path]);
    assert!(report.diagnostics.iter().any(|d| d.contains(path)));
    assert_eq!(vault.read(path).unwrap().bytes, edited);
    // The other conversation is still handled.
    assert_eq!(c.unchanged, 1);
}

#[test]
fn colliding_titles_get_the_conversation_id() {
    let (_tmp, vault) = setup();
    let conversations = json!([
        {"uuid": "aaaaaaaa-1", "name": "Same", "created_at": "2025-01-01T00:00:00Z",
         "chat_messages": [{"sender": "human", "text": "one"}]},
        {"uuid": "bbbbbbbb-2", "name": "Same", "created_at": "2025-01-01T09:00:00Z",
         "chat_messages": [{"sender": "human", "text": "two"}]},
        {"uuid": "cccccccc-3", "chat_messages": [{"sender": "human", "text": "no title or date"}]}
    ]);
    put(&vault, "c.json", bytes(&conversations));
    run(&vault, "c.json");
    assert_eq!(
        read(&vault, "AI/Claude/2025-01-01 Same.md").body(),
        "## User\n\none\n"
    );
    assert_eq!(
        read(&vault, "AI/Claude/2025-01-01 Same bbbbbbbb.md").body(),
        "## User\n\ntwo\n"
    );
    assert_eq!(
        read(&vault, "AI/Claude/Untitled.md").body(),
        "## User\n\nno title or date\n"
    );
}

#[test]
fn notion_style_zip_still_goes_to_the_notion_importer() {
    let (_tmp, vault) = setup();
    put(
        &vault,
        "export.zip",
        zip(&[("Home 11111111111111111111111111111111.md", b"# Home")]),
    );
    let report = run(&vault, "export.zip");
    assert!(report.conversations.is_none());
    assert!(report.sidecar_path.starts_with("Notion/"));
}

#[test]
fn an_unknown_array_in_an_archive_is_refused_with_a_sentence() {
    let (_tmp, vault) = setup();
    put(
        &vault,
        "x.zip",
        zip(&[("conversations.json", b"[{\"foo\": 1}]")]),
    );
    let err = import(&vault, "x.zip", ImportOptions::default()).unwrap_err();
    let sentence = wi_docs::conversations::owner_sentence(&err).expect("owner sentence");
    assert!(
        sentence.contains("Claude") && sentence.contains("ChatGPT"),
        "{sentence}"
    );
    assert!(!vault.root().join("AI").exists());
}

#[test]
fn an_empty_export_succeeds_with_nothing_created() {
    let (_tmp, vault) = setup();
    put(
        &vault,
        "e.zip",
        zip(&[("conversations.json", b"[]"), ("users.json", b"[]")]),
    );
    let report = run(&vault, "e.zip");
    assert_eq!(counts(&report), (0, 0, 0, 0, 0));
    assert!(report.unchanged);
}
