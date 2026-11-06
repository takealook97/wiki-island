use std::collections::BTreeMap;

use crate::provider::EVIDENCE_LABEL;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};

pub const MAX_BYTES: usize = 16384;
pub const MAX_HITS: usize = 8;

pub fn envelope(generation: u64, index_as_of: Option<String>, pending: bool) -> Value {
    json!({"label":EVIDENCE_LABEL,"generation":generation,"index_as_of":index_as_of,
        "index":if pending {"pending"} else {"ok"},"hits":[],"withheld":[],"diagnostics":[]})
}

pub fn withhold(value: &mut Value, reason: &str, count: usize) {
    if count == 0 {
        return;
    }
    // Provider and I/O diagnostics cannot use this channel to leak content.
    let reason = match reason {
        "stale"
        | "drift"
        | "conflict"
        | "condition-unknown"
        | "excluded"
        | "does-not-fit"
        | "lifecycle-unavailable"
        | "changed-after-index"
        | "generation-changed"
        | "hit-limit"
        | "not-current"
        | "source-unavailable"
        | "expired"
        | "archived"
        | "superseded"
        | "unsupported-file"
        | "unsupported-schema"
        | "diagnostics-omitted" => reason,
        _ => "not-current",
    };
    let Some(items) = value["withheld"].as_array_mut() else {
        return;
    };
    if let Some(item) = items.iter_mut().find(|v| v["reason"] == reason) {
        item["count"] = json!(
            item["count"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(count as u64)
        );
    } else {
        items.push(json!({"reason":reason,"count":count}));
    }
}

fn wrapped(value: Value, error: bool) -> CallToolResult {
    let content = vec![ContentBlock::text(value.to_string())];
    if error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}
fn size(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |b| b.len())
}

/// Fit ranked hits into `budget` bytes given the serialized size of the
/// envelope with an empty hit list. Returns the kept hits, each marked with
/// `presented`, and the number withheld as `does-not-fit`.
///
/// Deterministic: a hit that cannot fit even at its smallest tier is
/// withheld first; then, while over budget, the largest reducible hit (the
/// later one on ties) moves down one tier, and only when nothing can shrink
/// is the last hit dropped. A final pass, in rank order, gives each hit back
/// the richest tier the remaining room allows.
fn pack(hits: Vec<Value>, base: usize, budget: usize) -> (Vec<Value>, usize) {
    struct Slot {
        tiers: Vec<Value>,
        sizes: Vec<usize>,
        level: usize,
    }
    impl Slot {
        fn size(&self) -> usize {
            self.sizes[self.level]
        }
    }
    let mut dropped = 0;
    let mut slots: Vec<Slot> = Vec::new();
    for hit in hits {
        let tiers = crate::density::tiers(hit);
        let sizes: Vec<usize> = tiers.iter().map(size).collect();
        // Fits alone at its smallest tier, or never fits.
        if base + sizes[sizes.len() - 1] > budget {
            dropped += 1;
            continue;
        }
        slots.push(Slot {
            tiers,
            sizes,
            level: 0,
        });
    }
    let total = |slots: &[Slot]| {
        base + slots.iter().map(Slot::size).sum::<usize>() + slots.len().saturating_sub(1)
    };
    while total(&slots) > budget {
        let largest = slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.level + 1 < s.tiers.len())
            .max_by_key(|(i, s)| (s.size(), *i))
            .map(|(i, _)| i);
        match largest {
            Some(i) => slots[i].level += 1,
            // An empty slot list cannot shrink further; the caller fits the
            // non-hit envelope before packing, so this only guards the bound.
            None if slots.pop().is_none() => break,
            None => dropped += 1,
        }
    }
    for i in 0..slots.len() {
        let room = budget - total(&slots) + slots[i].size();
        if let Some(level) = (0..slots[i].level).find(|&l| slots[i].sizes[l] <= room) {
            slots[i].level = level;
        }
    }
    let kept = slots
        .into_iter()
        .map(|mut s| s.tiers.swap_remove(s.level))
        .collect();
    (kept, dropped)
}

/// Fit the non-hit rows (`items`, then `edges`) into `budget` before hits
/// are packed. A row that cannot fit even alone is withheld whole first;
/// then, while over budget, the last row is withheld. Rows are never
/// shortened. At most one pass per row, so this always terminates.
fn fit_rows(value: &mut Value, budget: usize) {
    const KEYS: [&str; 2] = ["items", "edges"];
    let mut empty = value.clone();
    for key in KEYS {
        if let Some(rows) = empty.get_mut(key).and_then(Value::as_array_mut) {
            rows.clear();
        }
    }
    let base = size(&empty);
    let mut withheld = 0;
    for key in KEYS {
        if let Some(rows) = value.get_mut(key).and_then(Value::as_array_mut) {
            let before = rows.len();
            rows.retain(|row| base + size(row) <= budget);
            withheld += before - rows.len();
        }
    }
    let units: usize = KEYS
        .iter()
        .map(|k| value[*k].as_array().map_or(0, Vec::len))
        .sum();
    for _ in 0..units {
        if size(value) <= budget {
            break;
        }
        let popped = KEYS.into_iter().any(|key| {
            value
                .get_mut(key)
                .and_then(Value::as_array_mut)
                .is_some_and(|a| a.pop().is_some())
        });
        if !popped {
            break;
        }
        withheld += 1;
    }
    withhold(value, "does-not-fit", withheld);
}

/// Cap one serialized structured result; the text block mirrors that value.
/// Never shorten a source span, qualifier, or metadata value. A plain note
/// that does not fit whole may be presented at a smaller computed tier
/// (`presented`); a lifecycle claim is delivered whole or withheld.
pub fn bounded(mut value: Value, error: bool) -> CallToolResult {
    for key in ["hits", "items", "edges"] {
        let len = value[key].as_array().map_or(0, Vec::len);
        if len > MAX_HITS {
            if let Some(items) = value[key].as_array_mut() {
                items.truncate(MAX_HITS);
            }
            withhold(&mut value, "hit-limit", len - MAX_HITS);
        }
    }
    let item_count = value["items"].as_array().map_or(0, Vec::len);
    let edge_count = value["edges"].as_array().map_or(0, Vec::len);
    if item_count + edge_count > MAX_HITS {
        if let Some(edges) = value.get_mut("edges").and_then(Value::as_array_mut) {
            edges.truncate(MAX_HITS.saturating_sub(item_count));
        }
        withhold(&mut value, "hit-limit", item_count + edge_count - MAX_HITS);
    }
    // Diagnostic text may contain note content or arbitrarily long I/O paths.
    // Only code strings are permitted here; details stay local.
    let mut codes = BTreeMap::<String, usize>::new();
    if let Some(ds) = value["diagnostics"].as_array() {
        for d in ds {
            let code = match d.as_str() {
                Some("generation-changed") => "generation-changed",
                Some("context-root-missing") => "context-root-missing",
                Some("context-root-ambiguous") => "context-root-ambiguous",
                Some("graph-truncated") => "graph-truncated",
                _ => "engine-diagnostic",
            };
            *codes.entry(code.into()).or_default() += 1;
        }
    }
    value["diagnostics"] = json!(
        codes
            .into_iter()
            .map(|(code, count)| json!({"code":code,"count":count}))
            .collect::<Vec<_>>()
    );
    // Reserve room for the final omission counter.
    let budget = MAX_BYTES - 256;
    let candidates = value["hits"].as_array_mut().map(std::mem::take);
    fit_rows(&mut value, budget);
    if let Some(candidates) = candidates {
        let (kept, dropped) = pack(candidates, size(&value), budget);
        value["hits"] = Value::Array(kept);
        withhold(&mut value, "does-not-fit", dropped);
    }
    let mut error_omitted = false;
    // Every pass removes one unit; the final reset always fits.
    let units = ["items", "edges", "hits"]
        .iter()
        .map(|k| value[*k].as_array().map_or(0, Vec::len))
        .sum::<usize>()
        + 3;
    for _ in 0..units {
        if size(&value) <= MAX_BYTES {
            break;
        }
        let removed = ["items", "edges"].into_iter().any(|key| {
            value
                .get_mut(key)
                .and_then(Value::as_array_mut)
                .is_some_and(|a| a.pop().is_some())
        });
        if removed {
            withhold(&mut value, "does-not-fit", 1);
            continue;
        }
        if let Some(hits) = value["hits"].as_array_mut()
            && hits.pop().is_some()
        {
            withhold(&mut value, "does-not-fit", 1);
            continue;
        }
        // A mutation receipt or provider payload too large for the envelope is
        // omitted as a unit. Keep success/error and generation visible.
        if value
            .as_object_mut()
            .is_some_and(|o| o.remove("data").is_some())
        {
            withhold(&mut value, "does-not-fit", 1);
            continue;
        }
        if !error_omitted
            && value
                .as_object_mut()
                .is_some_and(|o| o.remove("error").is_some())
        {
            value["error"] = json!({"code":"error-details-omitted"});
            error_omitted = true;
            continue;
        }
        break;
    }
    if size(&value) > MAX_BYTES {
        // Defensive boundary for future extensions.
        let generation = value["generation"].clone();
        value = envelope(0, None, true);
        value["generation"] = generation;
        value["index"] = Value::Null;
        withhold(&mut value, "does-not-fit", 1);
    }
    // Byte-budget omissions can remove graph nodes after traversal. Drop all
    // edges touching those nodes, including their target names.
    let retained: Vec<String> = value["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| h["path"].as_str().map(str::to_owned))
        .collect();
    if let Some(edges) = value.get_mut("edges").and_then(Value::as_array_mut) {
        edges.retain(|e| {
            e["from_path"]
                .as_str()
                .is_some_and(|p| retained.iter().any(|h| h == p))
                && e["to_path"]
                    .as_str()
                    .is_some_and(|p| retained.iter().any(|h| h == p))
        });
    }
    if let Some(hits) = value.get_mut("hits").and_then(Value::as_array_mut) {
        for hit in hits {
            if let Some(fields) = hit.as_object_mut() {
                fields.retain(|key, value| {
                    !((matches!(key.as_str(), "sources" | "status" | "claim_kind")
                        && value.is_null())
                        || (key == "fallback" && value == false)
                        || (key == "presented" && value == "full"))
                });
            }
        }
    }
    wrapped(value, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ToolResultText;

    fn bounded_within(value: Value, limit: std::time::Duration) -> Value {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(bounded(value, false).text_value());
        });
        rx.recv_timeout(limit)
            .expect("bounded() did not return")
            .expect("structured")
    }

    #[test]
    fn an_oversized_row_is_withheld_whole_and_packing_terminates() {
        let mut value = envelope(1, None, false);
        let title = "T".repeat(20_000);
        value["items"] = json!([
            {"path":"huge-title.md","kind":"file","size":1,"title":title},
            {"path":"small.md","kind":"file","size":1,"title":"Small"},
        ]);
        let start = std::time::Instant::now();
        let out = bounded_within(value, std::time::Duration::from_secs(5));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert!(size(&out) <= MAX_BYTES);
        assert_eq!(
            out["items"],
            json!([{"path":"small.md","kind":"file","size":1,"title":"Small"}])
        );
        assert_eq!(
            out["withheld"],
            json!([{"reason":"does-not-fit","count":1}])
        );
    }

    #[test]
    fn hits_still_pack_after_oversized_rows_are_withheld() {
        let mut value = envelope(1, None, false);
        value["items"] = json!([{"path":"x.md","title":"T".repeat(20_000)}]);
        value["hits"] = json!([{"path":"a.md","lifecycle":"none","content":"short","metadata":{}}]);
        let out = bounded_within(value, std::time::Duration::from_secs(5));
        assert_eq!(out["items"], json!([]));
        assert_eq!(out["hits"][0]["path"], "a.md");
        assert_eq!(
            out["withheld"],
            json!([{"reason":"does-not-fit","count":1}])
        );
    }
}
