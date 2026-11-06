//! Standing brief: one small, cheap read of what always applies. Pinned notes
//! (frontmatter `pinned: true`), then current rules, then current
//! preferences, newest first within each group, each at the summary tier.
//! The serialized brief never exceeds [`BRIEF_MAX_BYTES`]; entries that do
//! not fit are counted in `omitted`. Computed on demand; nothing is stored.

use std::time::UNIX_EPOCH;

use serde_json::{Map, Value, json};
use wi_note::Note;
use wi_store::{Query, SearchOptions, StoreError, Vault};

use crate::density::first_sentence;
use crate::provider::EVIDENCE_LABEL;

/// Hard cap of one serialized brief.
pub const BRIEF_MAX_BYTES: usize = 2048;
/// MCP resource address of the brief.
pub const BRIEF_URI: &str = "wiki-island://brief";
/// First pinned-candidate window; it doubles until the matches run out.
const PINNED_SCAN: usize = 64;
/// Largest pinned-candidate window read and classified in one brief.
const PINNED_MAX: usize = 10_000;
/// Pinned matches beyond [`PINNED_MAX`] are counted (not read) up to here and
/// reported in `omitted`.
const PINNED_COUNT_CAP: usize = 1_000_000;

struct Entry {
    group: u8,
    newest: i64,
    item: Value,
}

/// Build the brief for `vault`.
pub fn brief(vault: &Vault) -> wi_store::Result<Value> {
    let generation = vault.generation()?;
    let (mut entries, mut unread) = selected(vault, "pinned", json!(true), 0)?;
    entries.retain(|e| e.item["noted_by"] != "agent");
    let (noted, noted_unread) = selected(vault, "noted_by", json!("agent"), 3)?;
    unread += noted_unread;
    entries.extend(noted);
    let pinned_paths: Vec<String> = entries
        .iter()
        .filter_map(|e| e.item["path"].as_str().map(str::to_owned))
        .collect();
    if vault.lifecycle_on() {
        for rule in vault.current_rules()?.rules {
            if pinned_paths.contains(&rule.path) {
                continue;
            }
            let note = Note::parse(rule.content.as_bytes()).ok();
            let summary = rule
                .summary
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| note.as_ref().and_then(|n| first_sentence(n.body())))
                .unwrap_or_default();
            let group = if rule.kind == "rule" { 1 } else { 2 };
            entries.push(Entry {
                group,
                newest: newest(vault, &rule.path, note.as_ref()),
                item: item(&rule.kind, &rule.path, note.as_ref(), &summary),
            });
        }
    }
    entries.sort_by(|a, b| {
        a.group
            .cmp(&b.group)
            .then(b.newest.cmp(&a.newest))
            .then_with(|| a.item["path"].as_str().cmp(&b.item["path"].as_str()))
    });
    let mut out = json!({
        "label": EVIDENCE_LABEL,
        "generation": generation,
        "items": [],
        "noted_by_ai": {"label":"Noted by your AI", "items":[]},
        "omitted": 0,
        "max_bytes": BRIEF_MAX_BYTES,
    });
    let mut omitted = unread;
    // Size with the final omitted counter in place of the zero.
    let reserve = (entries.len() + unread).to_string().len();
    for entry in entries {
        let target = if entry.group == 3 {
            &mut out["noted_by_ai"]["items"]
        } else {
            &mut out["items"]
        };
        if let Some(items) = target.as_array_mut() {
            items.push(entry.item);
        }
        if serde_json::to_vec(&out).map_or(usize::MAX, |b| b.len()) + reserve > BRIEF_MAX_BYTES {
            let target = if entry.group == 3 {
                &mut out["noted_by_ai"]["items"]
            } else {
                &mut out["items"]
            };
            if let Some(items) = target.as_array_mut() {
                items.pop();
            }
            omitted += 1;
        }
    }
    out["omitted"] = json!(omitted);
    Ok(out)
}

/// Every eligible pinned note, and the number of pinned matches beyond
/// [`PINNED_MAX`] that were counted but not read.
fn selected(
    vault: &Vault,
    key: &str,
    value: Value,
    group: u8,
) -> wi_store::Result<(Vec<Entry>, usize)> {
    let mut filters = Map::new();
    filters.insert(key.into(), value.clone());
    let mut limit = PINNED_SCAN;
    // At most log2(PINNED_MAX / PINNED_SCAN) + 2 rounds.
    let response = loop {
        let query = Query {
            metadata_filters: filters.clone(),
            limit,
            count_limit: if limit == PINNED_MAX {
                PINNED_COUNT_CAP
            } else {
                0
            },
            ..Query::default()
        };
        let response = vault.search(&query, SearchOptions::default())?;
        if response.examined < limit || limit == PINNED_MAX {
            break response;
        }
        limit = limit.saturating_mul(2).min(PINNED_MAX);
    };
    let unread = response
        .matches
        .map_or(0, |m| m.saturating_sub(response.examined));
    let hits = response.hits;
    let mut read = Vec::new();
    for hit in hits {
        let path = hit.rel_path.to_string_lossy().into_owned();
        let Ok(current) = vault.read(&path) else {
            continue;
        };
        if current.checksum != hit.content_hash {
            continue;
        }
        let Ok(note) = Note::parse(&current.bytes) else {
            continue;
        };
        let is_pinned = note
            .frontmatter()
            .and_then(|f| f.get(key))
            .is_some_and(|v| v == value);
        if is_pinned {
            read.push((path, current.bytes));
        }
    }
    // A pinned claim is listed only while it is current.
    let labels = if read.is_empty() {
        Vec::new()
    } else {
        vault.classify(&read, &Value::Null)?
    };
    let mut out = Vec::new();
    for ((path, bytes), label) in read.into_iter().zip(labels) {
        if !label.is_deliverable() {
            continue;
        }
        let note = Note::parse(&bytes).map_err(StoreError::from)?;
        let summary = note
            .frontmatter()
            .and_then(|f| f.get("summary"))
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|s| !s.trim().is_empty())
            .or_else(|| first_sentence(note.body()))
            .unwrap_or_default();
        out.push(Entry {
            group,
            newest: newest(vault, &path, Some(&note)),
            item: item(
                if group == 3 { "noted" } else { "pinned" },
                &path,
                Some(&note),
                &summary,
            ),
        });
    }
    Ok((out, unread))
}

fn item(kind: &str, path: &str, note: Option<&Note>, summary: &str) -> Value {
    let title = note
        .and_then(|n| n.frontmatter())
        .and_then(|f| f.get("title"))
        .and_then(|v| v.as_str().map(str::to_owned));
    let mut value = json!({"kind": kind, "path": path, "title": title, "summary": summary});
    if let Some(noted_by) = note
        .and_then(|n| n.frontmatter())
        .and_then(|f| f.get("noted_by"))
    {
        value["noted_by"] = noted_by;
    }
    value
}

/// Seconds since the epoch of the note's own time (`observed_at`, `updated`,
/// `created` or `date`), else of the file's modification time.
fn newest(vault: &Vault, path: &str, note: Option<&Note>) -> i64 {
    let front = note.and_then(|n| n.frontmatter());
    for key in ["observed_at", "updated", "created", "date"] {
        let Some(text) = front
            .and_then(|f| f.get(key))
            .and_then(|v| v.as_str().map(str::to_owned))
        else {
            continue;
        };
        if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text.trim()) {
            return time.timestamp();
        }
        if let Some(time) = chrono::NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
        {
            return time.and_utc().timestamp();
        }
    }
    std::fs::symlink_metadata(vault.root().join(path))
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}
