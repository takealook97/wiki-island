//! The same compact JSON schemas drive discovery and runtime validation.
use rmcp::model::Tool;
use serde_json::{Map, Value, json};

fn string() -> Value {
    json!({"type":"string", "minLength":1})
}
fn boolean() -> Value {
    json!({"type":"boolean"})
}
fn object() -> Value {
    json!({"type":"object"})
}
fn strings() -> Value {
    json!({"type":"array", "items":string()})
}
fn number(min: u64, max: u64) -> Value {
    json!({"type":"integer", "minimum":min, "maximum":max})
}
fn enumeration(values: &[&str]) -> Value {
    json!({"type":"string", "enum":values})
}

fn tool(
    name: &'static str,
    description: &'static str,
    required: &[&str],
    fields: &[(&str, Value)],
    _read: bool,
) -> Tool {
    let mut properties: Map<String, Value> = fields
        .iter()
        .map(|(k, v)| ((*k).into(), v.clone()))
        .collect();
    if matches!(
        name,
        "wi_read" | "wi_search" | "wi_context" | "wi_list" | "wi_find"
    ) {
        properties.insert("read_mode".into(), enumeration(&["current", "all"]));
        properties.insert("conditions".into(), object());
    }
    if matches!(
        name,
        "wi_write"
            | "wi_edit"
            | "wi_move"
            | "wi_archive"
            | "wi_capture"
            | "wi_adopt"
            | "wi_work"
            | "wi_revert"
    ) {
        properties.insert("expected_checksum".into(), string());
        properties.insert("operation_id".into(), string());
    }
    if matches!(name, "wi_list" | "wi_find" | "wi_search" | "wi_history") {
        properties.insert("offset".into(), number(0, u32::MAX as u64));
        properties.insert("page".into(), number(1, u32::MAX as u64));
        properties.insert("limit".into(), number(1, 8));
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), json!(properties));
    if !required.is_empty() {
        schema.insert("required".into(), json!(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    Tool::new(name, description, schema)
}

pub fn tools() -> Vec<Tool> {
    vec![
        tool(
            "wi_read",
            "Read a note. Claims retain all qualifiers; optional section or line range.",
            &["identifier"],
            &[
                ("identifier", string()),
                ("section", string()),
                ("start_line", number(1, u32::MAX as u64)),
                ("end_line", number(1, u32::MAX as u64)),
                ("include_frontmatter", boolean()),
                ("include_content", boolean()),
            ],
            true,
        ),
        tool(
            "wi_write",
            "Create Markdown. Replacing requires expected_checksum. Agent origin is recorded.",
            &["identifier", "content"],
            &[
                ("identifier", string()),
                ("content", json!({"type":"string"})),
            ],
            false,
        ),
        tool(
            "wi_edit",
            "Edit metadata or body without rewriting unrelated bytes; claim revisions retain history.",
            &["identifier", "command"],
            &[
                ("identifier", string()),
                (
                    "command",
                    enumeration(&[
                        "append",
                        "prepend",
                        "find_replace",
                        "set_metadata",
                        "replace_section",
                        "insert_before_section",
                        "insert_after_section",
                    ]),
                ),
                ("content", json!({"type":"string"})),
                ("find_text", string()),
                ("expected_replacements", number(1, 1000000)),
                ("section", string()),
                ("replace_subsections", boolean()),
                ("metadata", object()),
                ("remove", strings()),
            ],
            false,
        ),
        tool(
            "wi_move",
            "Move a note or directory; trailing / means a folder. Directory progress may be partial.",
            &["destination"],
            &[
                ("identifier", string()),
                ("path", string()),
                ("destination", string()),
            ],
            false,
        ),
        tool(
            "wi_archive",
            "Archive a note; history is retained.",
            &["identifier"],
            &[("identifier", string())],
            false,
        ),
        tool(
            "wi_search",
            "Search local text, titles, permalinks or metadata; at most eight hits.",
            &[],
            &[
                ("query", json!({"type":"string"})),
                ("mode", enumeration(&["text", "title", "permalink"])),
                ("filters", object()),
                ("note_type", string()),
                ("categories", strings()),
                ("limit", number(1, 8)),
            ],
            true,
        ),
        tool(
            "wi_context",
            "Traverse links from a note; since filters related notes, not the root.",
            &["identifier"],
            &[
                ("identifier", string()),
                ("depth", number(0, 16)),
                ("since", string()),
                ("max_related", number(0, 7)),
            ],
            true,
        ),
        tool(
            "wi_list",
            "List files and folders; sort folders first. glob matches basenames.",
            &[],
            &[
                ("path", string()),
                ("glob", string()),
                (
                    "sort",
                    enumeration(&["title_asc", "title_desc", "updated_asc", "updated_desc"]),
                ),
                ("depth", number(1, 10)),
                ("limit", number(1, 8)),
            ],
            true,
        ),
        tool(
            "wi_find",
            "Find by basename, metadata filters or note type. fields projects metadata.",
            &[],
            &[
                ("path", string()),
                ("name", string()),
                ("filters", object()),
                ("fields", strings()),
                ("depth", number(1, 10)),
                ("limit", number(1, 8)),
            ],
            true,
        ),
        tool(
            "wi_history",
            "List committed revisions of a note.",
            &["identifier"],
            &[("identifier", string()), ("command", enumeration(&["log"]))],
            true,
        ),
        tool(
            "wi_revert",
            "Restore a committed revision. Claims restore as new notes.",
            &["identifier", "commit"],
            &[("identifier", string()), ("commit", string())],
            false,
        ),
        tool(
            "wi_capture",
            "Capture text, optional host-fetched snapshot and candidate notes. Never fetches URLs.",
            &["content"],
            &[
                ("content", json!({"type":"string"})),
                ("title", string()),
                ("no_memory_reason", string()),
                (
                    "snapshot",
                    json!({"type":"object","properties":{"url":string(),"path":{"type":"string","minLength":1,"description":"Vault-relative snapshot file."},"content":{"type":"string"}},"required":["url","path","content"],"additionalProperties":false}),
                ),
                (
                    "candidates",
                    json!({"type":"array","items":{"type":"object","properties":{"path":{"type":"string","minLength":1,"description":"Vault-relative candidate note."},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false}}),
                ),
            ],
            false,
        ),
        tool(
            "wi_adopt",
            "Rerun lifecycle gates on a candidate and apply the outcome.",
            &["identifier"],
            &[("identifier", string())],
            false,
        ),
        tool(
            "wi_lint",
            "Check one or all memory notes without changing them.",
            &[],
            &[("identifier", string())],
            true,
        ),
        tool(
            "wi_work",
            "Next tasks (3 items/4096 bytes), list/open/close/sweep. Unsupported closure needs limits and a reopen trigger.",
            &["action"],
            &[
                (
                    "action",
                    enumeration(&["next", "list", "open", "close", "sweep"]),
                ),
                ("max_items", number(1, 8)),
                ("max_bytes", number(64, 12288)),
                ("identifier", string()),
                ("target", string()),
                ("reason", string()),
                (
                    "outcome",
                    enumeration(&[
                        "adopted",
                        "narrowed",
                        "alternative",
                        "not_supported",
                        "evidence_unavailable",
                        "closed_without_supported_answer",
                    ]),
                ),
                ("limits", string()),
                (
                    "reopen_trigger",
                    enumeration(&["session", "source-change", "query", "cooldown"]),
                ),
                ("reopen_detail", string()),
            ],
            false,
        ),
    ]
}

/// Validate precisely the schema subset used above, including closed argument
/// objects. Avoid silently accepting a misspelled safety precondition.
pub fn validate(schema: &Value, value: &Value) -> bool {
    if schema
        .get("allOf")
        .and_then(Value::as_array)
        .is_some_and(|schemas| schemas.iter().any(|s| !validate(s, value)))
        || schema
            .get("anyOf")
            .and_then(Value::as_array)
            .is_some_and(|schemas| !schemas.iter().any(|s| validate(s, value)))
    {
        return false;
    }
    if schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|keys| {
            keys.iter()
                .any(|key| key.as_str().is_none_or(|key| value.get(key).is_none()))
        })
    {
        return false;
    }
    if let Some(choices) = schema.get("enum").and_then(Value::as_array)
        && !choices.contains(value)
    {
        return false;
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let Some(map) = value.as_object() else {
                return false;
            };
            let props = schema.get("properties").and_then(Value::as_object);
            if schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|req| {
                    req.iter()
                        .any(|k| k.as_str().is_none_or(|k| !map.contains_key(k)))
                })
            {
                return false;
            }
            for (k, v) in map {
                if let Some(s) = props.and_then(|p| p.get(k)) {
                    if !validate(s, v) {
                        return false;
                    }
                } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                    return false;
                }
            }
        }
        Some("string") => {
            let Some(s) = value.as_str() else {
                return false;
            };
            if schema
                .get("minLength")
                .and_then(Value::as_u64)
                .is_some_and(|n| s.chars().count() < n as usize)
            {
                return false;
            }
        }
        Some("integer") => {
            let Some(n) = integer(value) else {
                return false;
            };
            if schema
                .get("minimum")
                .and_then(Value::as_u64)
                .is_some_and(|m| n < m)
                || schema
                    .get("maximum")
                    .and_then(Value::as_u64)
                    .is_some_and(|m| n > m)
            {
                return false;
            }
        }
        Some("boolean") => {
            if !value.is_boolean() {
                return false;
            }
        }
        Some("array") => {
            let Some(items) = value.as_array() else {
                return false;
            };
            if let Some(s) = schema.get("items")
                && items.iter().any(|v| !validate(s, v))
            {
                return false;
            }
        }
        _ => {}
    }
    true
}

/// JSON Schema integers include integral JSON floating-point numbers.
pub fn integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n < u64::MAX as f64)
            .map(|n| n as u64)
    })
}

pub fn aliases(name: &str) -> Vec<(&'static str, &'static str)> {
    let mut result = Vec::new();
    if matches!(
        name,
        "wi_read"
            | "wi_write"
            | "wi_edit"
            | "wi_move"
            | "wi_archive"
            | "wi_context"
            | "wi_history"
            | "wi_revert"
            | "wi_adopt"
            | "wi_lint"
    ) {
        result.push(("file_path", "identifier"));
    }
    match name {
        "wi_context" => result.push(("url", "identifier")),
        "wi_move" => result.extend([
            ("destination_path", "destination"),
            ("destination_folder", "destination"),
            ("to", "destination"),
            ("new_path", "destination"),
            ("dir_name", "path"),
            ("folder", "path"),
        ]),
        "wi_edit" => result.extend([
            ("operation", "command"),
            ("new_string", "content"),
            ("new_content", "content"),
            ("replacement", "content"),
            ("replace_with", "content"),
            ("heading", "section"),
            ("section_heading", "section"),
            ("old_string", "find_text"),
            ("find", "find_text"),
            ("old_text", "find_text"),
            ("old_content", "find_text"),
            ("search", "find_text"),
        ]),
        "wi_search" => result.extend([
            ("q", "query"),
            ("text", "query"),
            ("pattern", "query"),
            ("search_type", "mode"),
            ("category", "categories"),
        ]),
        "wi_work" => result.push(("command", "action")),
        "wi_list" => result.extend([
            ("file_name_glob", "glob"),
            ("pattern", "glob"),
            ("page_size", "limit"),
        ]),
        _ => {}
    }
    result
}

pub fn normalize(name: &str, args: &mut Value) -> bool {
    let Some(map) = args.as_object_mut() else {
        return false;
    };
    for (alias, canonical) in aliases(name) {
        if let Some(value) = map.remove(alias) {
            if map.get(canonical).is_some_and(|current| current != &value) {
                return false;
            }
            map.insert(canonical.into(), value);
        }
    }
    if matches!(
        map.get("read_mode").and_then(Value::as_str),
        Some("historic" | "explore")
    ) {
        map.insert("read_mode".into(), json!("all"));
    }
    true
}

/// Discovery omits validation-only constraints and repeated prose. Runtime
/// validation uses the full schemas above; tools keep their fields and types.
pub fn discovery_tools() -> Vec<Tool> {
    fn compact(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for key in ["minLength", "minimum", "maximum", "additionalProperties"] {
                    map.remove(key);
                }
                for value in map.values_mut() {
                    compact(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    compact(value);
                }
            }
            _ => {}
        }
    }
    tools()
        .into_iter()
        .map(|mut tool| {
            let mut value = json!(tool.input_schema);
            compact(&mut value);
            tool.input_schema = std::sync::Arc::new(value.as_object().cloned().unwrap_or_default());
            tool
        })
        .collect()
}
