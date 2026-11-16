//! ChatGPT `conversations.json`: a tree of message nodes per conversation,
//! of which only the branch ending at `current_node` is the visible thread.
use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;

use super::{Conversation, Message, Role, fallback_id, from_epoch, lenient};

#[derive(Deserialize, Default)]
#[serde(default)]
struct Conv {
    #[serde(deserialize_with = "lenient::string")]
    id: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    conversation_id: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    title: Option<String>,
    #[serde(deserialize_with = "lenient::number")]
    create_time: Option<f64>,
    #[serde(deserialize_with = "lenient::number")]
    update_time: Option<f64>,
    #[serde(deserialize_with = "lenient::string")]
    current_node: Option<String>,
    mapping: BTreeMap<String, Node>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Node {
    #[serde(deserialize_with = "lenient::string")]
    parent: Option<String>,
    #[serde(deserialize_with = "lenient::list")]
    children: Vec<String>,
    message: Option<Msg>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Msg {
    author: Option<Author>,
    #[serde(deserialize_with = "lenient::number")]
    create_time: Option<f64>,
    content: Option<Content>,
    metadata: Option<Meta>,
    #[serde(deserialize_with = "lenient::string")]
    recipient: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Author {
    #[serde(deserialize_with = "lenient::string")]
    role: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Content {
    #[serde(deserialize_with = "lenient::string")]
    content_type: Option<String>,
    #[serde(deserialize_with = "lenient::list")]
    parts: Vec<Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Meta {
    is_visually_hidden_from_conversation: Option<bool>,
    #[serde(deserialize_with = "lenient::list")]
    attachments: Vec<Named>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Named {
    #[serde(deserialize_with = "lenient::string")]
    name: Option<String>,
}

/// Node ids from the root to the visible leaf.
fn thread(conv: &Conv) -> Vec<&str> {
    let start = conv
        .current_node
        .as_deref()
        .filter(|id| conv.mapping.contains_key(*id))
        .or_else(|| latest_leaf(conv));
    let mut chain = Vec::new();
    let mut seen = BTreeSet::new();
    let mut next = start;
    while let Some(id) = next {
        if !seen.insert(id) {
            break;
        }
        let Some(node) = conv.mapping.get(id) else {
            break;
        };
        chain.push(id);
        next = node.parent.as_deref();
    }
    chain.reverse();
    chain
}

/// The childless node with the latest message time.
fn latest_leaf(conv: &Conv) -> Option<&str> {
    conv.mapping
        .iter()
        .filter(|(_, node)| node.children.iter().all(|c| !conv.mapping.contains_key(c)))
        .max_by(|a, b| {
            let time = |n: &Node| {
                n.message
                    .as_ref()
                    .and_then(|m| m.create_time)
                    .unwrap_or(0.0)
            };
            time(a.1).total_cmp(&time(b.1)).then_with(|| b.0.cmp(a.0))
        })
        .map(|(id, _)| id.as_str())
}

fn role(msg: &Msg) -> Option<Role> {
    match msg.author.as_ref()?.role.as_deref()? {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

fn visible(msg: &Msg) -> bool {
    msg.recipient.as_deref().is_none_or(|r| r == "all")
        && msg
            .metadata
            .as_ref()
            .and_then(|m| m.is_visually_hidden_from_conversation)
            != Some(true)
}

fn message(msg: &Msg) -> Option<Message> {
    let role = role(msg)?;
    if !visible(msg) {
        return None;
    }
    let content = msg.content.as_ref()?;
    if !matches!(
        content.content_type.as_deref(),
        Some("text" | "multimodal_text")
    ) {
        return None;
    }
    let mut strings = Vec::new();
    let mut attachments: Vec<String> = Vec::new();
    for part in &content.parts {
        match part {
            Value::String(text) if !text.trim().is_empty() => strings.push(text.as_str()),
            Value::String(_) | Value::Null => {}
            other => attachments.push(part_name(other)),
        }
    }
    if let Some(meta) = &msg.metadata {
        attachments.extend(meta.attachments.iter().filter_map(|a| a.name.clone()));
    }
    let mut unique: Vec<String> = Vec::new();
    for name in attachments {
        if !unique.contains(&name) {
            unique.push(name);
        }
    }
    let text = strings.join("\n\n");
    (!text.trim().is_empty() || !unique.is_empty()).then_some(Message {
        role,
        text,
        attachments: unique,
    })
}

fn part_name(part: &Value) -> String {
    ["name", "file_name"]
        .iter()
        .find_map(|key| part.get(key).and_then(Value::as_str))
        .filter(|n| !n.trim().is_empty())
        .unwrap_or("image")
        .to_owned()
}

/// `None`: the element is not a readable conversation.
pub(super) fn conversation(value: Value) -> Option<Conversation> {
    let conv: Conv = serde_json::from_value(value).ok()?;
    let created = conv.create_time.and_then(from_epoch);
    let title = conv.title.clone().unwrap_or_default();
    let messages = thread(&conv)
        .into_iter()
        .filter_map(|id| conv.mapping.get(id)?.message.as_ref())
        .filter_map(message)
        .collect();
    let id = conv
        .conversation_id
        .clone()
        .or_else(|| conv.id.clone())
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| fallback_id(&title, created));
    Some(Conversation {
        id,
        title,
        created,
        updated: conv.update_time.and_then(from_epoch),
        messages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(parent: Option<&str>, children: &[&str], message: Value) -> Value {
        json!({"id": "x", "parent": parent, "children": children, "message": message})
    }

    fn msg(role: &str, kind: &str, parts: Value, time: f64) -> Value {
        json!({"id": "m", "author": {"role": role}, "create_time": time,
               "content": {"content_type": kind, "parts": parts}, "metadata": {}, "recipient": "all"})
    }

    #[test]
    fn only_the_current_branch_and_visible_messages_appear() {
        let mut hidden = msg("assistant", "text", json!(["hidden"]), 6.0);
        hidden["metadata"] = json!({"is_visually_hidden_from_conversation": true});
        let mut to_tool = msg("assistant", "text", json!(["search()"]), 7.0);
        to_tool["recipient"] = json!("browser");
        let conv = conversation(json!({
            "conversation_id": "g1", "title": "Branches", "create_time": 1_700_000_000.5,
            "update_time": null, "current_node": "b2",
            "mapping": {
                "root": node(None, &["sys"], Value::Null),
                "sys": node(Some("root"), &["u1"], msg("system", "text", json!(["You are"]), 1.0)),
                "u1": node(Some("sys"), &["a1", "a1b"], msg("user", "text", json!(["Question"]), 2.0)),
                "a1": node(Some("u1"), &[], msg("assistant", "text", json!(["Old answer"]), 3.0)),
                "a1b": node(Some("u1"), &["t1"], msg("assistant", "text", json!(["Answer"]), 4.0)),
                "t1": node(Some("a1b"), &["h", "tool", "code"], msg("tool", "text", json!(["tool out"]), 5.0)),
                "h": node(Some("t1"), &[], hidden),
                "tool": node(Some("t1"), &[], to_tool),
                "code": node(Some("t1"), &["b2"], msg("assistant", "code", json!(["print(1)"]), 8.0)),
                "b2": node(Some("code"), &[], msg("user", "multimodal_text",
                    json!([{"content_type": "image_asset_pointer"}, "Look at this"]), 9.0))
            }
        }))
        .unwrap();
        assert_eq!(conv.id, "g1");
        assert!(conv.updated.is_none());
        let texts: Vec<(&Role, &str)> = conv
            .messages
            .iter()
            .map(|m| (&m.role, m.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            [
                (&Role::User, "Question"),
                (&Role::Assistant, "Answer"),
                (&Role::User, "Look at this")
            ]
        );
        assert_eq!(conv.messages[2].attachments, ["image"]);
    }

    #[test]
    fn without_a_current_node_the_latest_leaf_is_used() {
        let conv = conversation(json!({
            "id": "g2", "title": "T", "create_time": null,
            "mapping": {
                "r": node(None, &["a", "b"], Value::Null),
                "a": node(Some("r"), &[], msg("user", "text", json!(["early"]), 1.0)),
                "b": node(Some("r"), &[], msg("user", "text", json!(["late"]), 2.0))
            }
        }))
        .unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].text, "late");
    }

    #[test]
    fn a_parent_cycle_ends_the_walk_and_a_wrong_shape_is_unreadable() {
        let conv = conversation(json!({
            "id": "g3", "current_node": "a",
            "mapping": {"a": node(Some("b"), &[], msg("user", "text", json!(["x"]), 1.0)),
                         "b": node(Some("a"), &[], msg("assistant", "text", json!(["y"]), 2.0))}
        }))
        .unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert!(conversation(json!({"mapping": []})).is_none());
    }
}
