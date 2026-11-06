//! Whether a publication during a read touched what the read returned.
//!
//! A hit-shaped read (search, read, context, metadata find) depends on the
//! returned notes, the paths their qualifiers name (sources, supersedes,
//! conflicts, restores) and, through eligibility, on every lifecycle note.
//! A publication of anything else, such as a rules export or an unrelated
//! plain note, leaves the read consistent and must not withhold it.

use std::collections::BTreeSet;

use serde_json::Value;
use wi_note::{LifecycleType, Note};
use wi_store::{PublicationMark, Vault};

/// Tools whose output is fully described by hit and edge paths.
const HIT_TOOLS: [&str; 5] = ["wi_read", "wi_search", "wi_context", "wi_list", "wi_find"];
/// Frontmatter keys whose values name other files a delivery depends on.
const QUALIFIER_KEYS: [&str; 4] = ["sources", "supersedes", "conflict_with", "restores"];

/// True when a change after `start` may affect `out`; unknown is true.
pub(crate) fn touched(vault: &Vault, tool: &str, start: &PublicationMark, out: &Value) -> bool {
    let rows = out.get("items").and_then(Value::as_array);
    if !HIT_TOOLS.contains(&tool) || rows.is_some_and(|r| !r.is_empty()) {
        return true;
    }
    let Ok(Some(changed)) = vault.changed_since(start) else {
        return true;
    };
    let deps = dependencies(out);
    changed.iter().any(|path| {
        deps.contains(path) || path.starts_with(".wiki-island/") || lifecycle(vault, path)
    })
}

/// Paths of returned hits and edges, and every string their qualifiers hold.
fn dependencies(out: &Value) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let hits = out.get("hits").and_then(Value::as_array);
    for hit in hits.into_iter().flatten() {
        if let Some(path) = hit["path"].as_str() {
            deps.insert(path.to_owned());
        }
        for key in QUALIFIER_KEYS {
            strings(&hit["metadata"][key], &mut deps);
        }
        strings(&hit["sources"], &mut deps);
    }
    let edges = out.get("edges").and_then(Value::as_array);
    for edge in edges.into_iter().flatten() {
        for key in ["from_path", "to_path", "from", "to"] {
            if let Some(path) = edge[key].as_str() {
                deps.insert(path.to_owned());
            }
        }
    }
    deps
}

fn strings(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::String(s) => {
            out.insert(s.clone());
        }
        Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

/// A changed Markdown file counts unless it now reads as a plain note:
/// lifecycle notes feed eligibility of every hit, and a deleted or
/// unreadable one cannot be shown to be unrelated.
fn lifecycle(vault: &Vault, path: &str) -> bool {
    if !path.to_ascii_lowercase().ends_with(".md") {
        return false;
    }
    let Ok(read) = vault.read(path) else {
        return true;
    };
    Note::parse(&read.bytes).map_or(true, |note| {
        note.lifecycle().lifecycle_type != LifecycleType::None
    })
}
