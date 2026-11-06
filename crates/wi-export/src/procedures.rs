//! Stable skill-id resources; a direct lookup is explicitly labelled explore.
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde_json::{Value, json};
use wi_store::{ProcedureMode, StoreError, Vault};

fn uri(id: &str) -> String {
    format!(
        "wiki-island://procedures/{}",
        utf8_percent_encode(id, NON_ALPHANUMERIC)
    )
}
fn entries(vault: &Vault, mode: ProcedureMode) -> wi_store::Result<Vec<Value>> {
    vault.catch_up()?;
    let mut entries = Vec::new();
    for label in vault.procedures(mode)? {
        let read = vault.read(&label.path)?;
        let note = wi_note::Note::parse(&read.bytes)?;
        let fresh = vault.classify(&[(label.path.clone(), read.bytes.clone())], &Value::Null)?;
        let Some(label) = fresh.into_iter().next() else {
            continue;
        };
        if mode == ProcedureMode::Discovery && label.eligibility != "current" {
            continue;
        }
        let lifecycle = note.lifecycle();
        if lifecycle.lifecycle_type != wi_note::LifecycleType::Memory
            || lifecycle.kind != Some(wi_note::Kind::Procedure)
        {
            continue;
        }
        let Some(id) = lifecycle.skill_id.as_deref().filter(|id| !id.is_empty()) else {
            continue;
        };
        let current = label.eligibility == "current";
        let stale = label
            .freshness
            .pointer("/state/state")
            .and_then(Value::as_str)
            .is_some_and(|state| !matches!(state, "fresh" | "timeless"));
        entries.push(json!({
            "uri": uri(id), "name": id, "mimeType":"application/json",
            "description": "Procedure evidence; check eligibility before executing.",
            "path": label.path, "status": lifecycle.status,
            "freshness":label.freshness, "eligibility": label.eligibility,
            "mode": if current { "current" } else { "explore" },
            "excluded": lifecycle.excluded.unwrap_or(false), "exclusion_reason": lifecycle.exclusion_reason,
            "stale": stale, "do_not_execute": !current, "reason": label.reason,
            "sources": {"note": label.path, "checksum": read.checksum, "refs": lifecycle.sources, "spans":label.spans},
            "evidence_not_instruction":true, "text": note.body()
        }));
    }
    entries.sort_by(|a, b| a["uri"].as_str().cmp(&b["uri"].as_str()));
    Ok(entries)
}
const URI_PREFIX: &str = "wiki-island://procedures/";
/// Decode a client URI to its skill id. Scheme and authority compare
/// case-insensitively and the id is percent-decoded, so RFC 3986-equivalent
/// spellings (`deploy-x`, `deploy%2Dx`, `deploy%2dx`) name the same resource.
fn requested_id(target_uri: &str) -> Option<String> {
    let prefix = target_uri.get(..URI_PREFIX.len())?;
    if !prefix.eq_ignore_ascii_case(URI_PREFIX) {
        return None;
    }
    let encoded = &target_uri[URI_PREFIX.len()..];
    if encoded.is_empty() || encoded.contains(['?', '#']) {
        return None;
    }
    let id = percent_decode_str(encoded).decode_utf8().ok()?;
    Some(id.into_owned())
}
fn is_terminal(entry: &Value) -> bool {
    matches!(
        entry["status"].as_str(),
        Some("superseded" | "rejected" | "archived")
    )
}
/// Resolve one identity. Terminal predecessors are retained history, not
/// competitors: a single current (else single non-terminal) entry wins.
/// Genuinely competing entries are refused, never silently chosen.
fn resolve(mut candidates: Vec<Value>) -> wi_store::Result<Option<Value>> {
    if candidates.len() <= 1 {
        return Ok(candidates.pop());
    }
    let ambiguous =
        || StoreError::Invalid("duplicate procedure skill_id; resolve the ambiguity".into());
    let current: Vec<_> = candidates
        .iter()
        .filter(|e| e["eligibility"] == "current")
        .collect();
    match current.len() {
        1 => return Ok(Some(current[0].clone())),
        0 => {}
        _ => return Err(ambiguous()),
    }
    let live: Vec<_> = candidates.iter().filter(|e| !is_terminal(e)).collect();
    match live.len() {
        1 => Ok(Some(live[0].clone())),
        _ => Err(ambiguous()),
    }
}
pub fn list(vault: &Vault) -> wi_store::Result<Value> {
    let entries = entries(vault, ProcedureMode::Discovery)?;
    // Discovery lists current entries only; an id with competing current
    // entries is omitted (its direct read is refused) without hiding others.
    let resources = entries
        .iter()
        .filter(|v| entries.iter().filter(|o| o["uri"] == v["uri"]).count() == 1)
        .map(|v| json!({"uri":v["uri"],"name":v["name"],"mimeType":v["mimeType"],"description":v["description"]}))
        .collect::<Vec<_>>();
    Ok(json!({ "resources": resources }))
}
pub fn read(vault: &Vault, target_uri: &str) -> wi_store::Result<Value> {
    let not_found = || StoreError::NotFound {
        path: target_uri.into(),
    };
    let id = requested_id(target_uri).ok_or_else(not_found)?;
    let candidates = entries(vault, ProcedureMode::Direct)?
        .into_iter()
        .filter(|v| v["name"] == id.as_str())
        .collect();
    let entry = resolve(candidates)?.ok_or_else(not_found)?;
    let text = serde_json::to_string(&entry).map_err(|e| StoreError::Invalid(e.to_string()))?;
    if text.len() > 1024 * 1024 {
        return Err(StoreError::Invalid(
            "procedure exceeds 1 MiB resource cap; read the source file".into(),
        ));
    }
    Ok(json!({"contents":[{"uri":target_uri,"mimeType":"application/json","text":text}]}))
}
