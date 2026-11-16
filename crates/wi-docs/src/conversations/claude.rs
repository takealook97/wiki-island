//! Claude `conversations.json` and `projects.json`.
use serde::Deserialize;
use serde_json::Value;

use super::{Conversation, Message, ProjectDoc, Role, fallback_id, from_rfc3339, lenient};

#[derive(Deserialize, Default)]
#[serde(default)]
struct Conv {
    #[serde(deserialize_with = "lenient::string")]
    uuid: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    name: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    created_at: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    updated_at: Option<String>,
    #[serde(deserialize_with = "lenient::list")]
    chat_messages: Vec<Msg>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Msg {
    #[serde(deserialize_with = "lenient::string")]
    sender: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    text: Option<String>,
    #[serde(deserialize_with = "lenient::list")]
    content: Vec<Block>,
    #[serde(deserialize_with = "lenient::list")]
    attachments: Vec<File>,
    #[serde(deserialize_with = "lenient::list")]
    files: Vec<File>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Block {
    #[serde(rename = "type", deserialize_with = "lenient::string")]
    kind: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    text: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct File {
    #[serde(deserialize_with = "lenient::string")]
    file_name: Option<String>,
}

fn role(sender: &str) -> Option<Role> {
    match sender {
        "human" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

/// Text blocks joined by a blank line; the plain `text` field when the
/// message has no text block.
fn text(msg: &Msg) -> String {
    let blocks: Vec<&str> = msg
        .content
        .iter()
        .filter(|b| b.kind.as_deref() == Some("text"))
        .filter_map(|b| b.text.as_deref())
        .filter(|t| !t.trim().is_empty())
        .collect();
    if blocks.is_empty() {
        msg.text.clone().unwrap_or_default()
    } else {
        blocks.join("\n\n")
    }
}

fn attachments(msg: &Msg) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in msg
        .attachments
        .iter()
        .chain(&msg.files)
        .filter_map(|f| f.file_name.as_deref())
        .filter(|n| !n.trim().is_empty())
    {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    }
    names
}

/// `None`: the element is not a readable conversation.
pub(super) fn conversation(value: Value) -> Option<Conversation> {
    let conv: Conv = serde_json::from_value(value).ok()?;
    let created = conv.created_at.as_deref().and_then(from_rfc3339);
    let title = conv.name.unwrap_or_default();
    let messages = conv
        .chat_messages
        .iter()
        .filter_map(|msg| {
            let role = role(msg.sender.as_deref()?)?;
            let text = text(msg);
            let attachments = attachments(msg);
            (!text.trim().is_empty() || !attachments.is_empty()).then_some(Message {
                role,
                text,
                attachments,
            })
        })
        .collect();
    Some(Conversation {
        id: conv
            .uuid
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| fallback_id(&title, created)),
        title,
        created,
        updated: conv.updated_at.as_deref().and_then(from_rfc3339),
        messages,
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Project {
    #[serde(deserialize_with = "lenient::string")]
    uuid: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    name: Option<String>,
    #[serde(deserialize_with = "lenient::list")]
    docs: Vec<Doc>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Doc {
    #[serde(deserialize_with = "lenient::string")]
    uuid: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    filename: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    content: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    created_at: Option<String>,
}

/// Documents of one project element; `None` when the element is unreadable.
/// Documents without content are returned with empty content so the caller
/// can count them as skipped.
pub(super) fn project_docs(value: Value) -> Option<Vec<ProjectDoc>> {
    let project: Project = serde_json::from_value(value).ok()?;
    let name = project.name.unwrap_or_default();
    let project_id = project
        .uuid
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| fallback_id(&name, None));
    Some(
        project
            .docs
            .into_iter()
            .enumerate()
            .map(|(index, doc)| {
                let filename = doc.filename.unwrap_or_default();
                ProjectDoc {
                    id: doc
                        .uuid
                        .filter(|id| !id.trim().is_empty())
                        .unwrap_or_else(|| fallback_id(&format!("{filename}#{index}"), None)),
                    project: name.clone(),
                    project_id: project_id.clone(),
                    filename,
                    content: doc.content.unwrap_or_default(),
                    created: doc.created_at.as_deref().and_then(from_rfc3339),
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_text_blocks_attachments_and_skips_other_senders() {
        let conv = conversation(json!({
            "uuid": "c1", "name": "Plan", "created_at": "2025-03-04T05:06:07.123456Z",
            "updated_at": "2025-03-05T00:00:00Z", "unknown": [1, 2],
            "chat_messages": [
                {"sender": "human", "text": "ignored",
                 "content": [{"type": "text", "text": "Hello"}],
                 "attachments": [{"file_name": "a.pdf"}], "files": [{"file_name": "a.pdf"}, {"file_name": "b.png"}]},
                {"sender": "assistant", "text": "Hi there", "content": []},
                {"sender": "assistant", "content": [
                    {"type": "thinking", "text": "hidden"},
                    {"type": "tool_use", "name": "x"},
                    {"type": "text", "text": "One"}, {"type": "text", "text": "Two"}]},
                {"sender": "system", "text": "nope"},
                {"sender": "human", "text": "  "}
            ]
        }))
        .unwrap();
        assert_eq!(conv.id, "c1");
        assert_eq!(
            conv.created.unwrap().to_rfc3339(),
            "2025-03-04T05:06:07.123456+00:00"
        );
        let texts: Vec<&str> = conv.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["Hello", "Hi there", "One\n\nTwo"]);
        assert_eq!(conv.messages[0].attachments, ["a.pdf", "b.png"]);
        assert_eq!(conv.messages[1].role, Role::Assistant);
    }

    #[test]
    fn missing_keys_are_tolerated_and_a_wrong_shape_is_unreadable() {
        let conv = conversation(json!({"chat_messages": null})).unwrap();
        assert!(conv.messages.is_empty());
        assert!(conv.id.starts_with("anon-"));
        assert!(conversation(json!("text")).is_none());
        assert!(conversation(json!({"chat_messages": "x"})).is_none());
    }

    #[test]
    fn project_documents_carry_their_project() {
        let docs = project_docs(json!({
            "uuid": "p1", "name": "Alpha",
            "docs": [{"uuid": "d1", "filename": "notes.txt", "content": "Body"}, {"filename": "empty.md"}]
        }))
        .unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].project, "Alpha");
        assert_eq!(docs[0].content, "Body");
        assert!(docs[1].content.is_empty());
        assert!(project_docs(json!(7)).is_none());
    }
}
