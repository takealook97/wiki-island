//! Human explanation of an adoption outcome (`wi write`, `edit`, `move`,
//! `archive` and `adopt`).

use serde_json::Value;

/// Explain a lifecycle result shaped `{"adoption": {"outcome": ..}}`.
/// Returns `None` for plain writes and active outcomes.
pub(crate) fn explain(lifecycle: Option<&Value>) -> Option<String> {
    let adoption = lifecycle?.get("adoption")?;
    match adoption.get("outcome")?.as_str()? {
        "candidate" => Some(candidate(adoption)),
        "deferred" => Some("deferred: change cap reached; it resumes on its own".to_owned()),
        "rejected" => Some(format!("rejected: {}", reason_text(adoption.get("reason")))),
        _ => None,
    }
}

fn reason_text(reason: Option<&Value>) -> String {
    match reason {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => "no reason recorded".to_owned(),
        Some(other) => other.to_string(),
    }
}

fn candidate(adoption: &Value) -> String {
    let mut lines = vec!["candidate: the engine kept this claim out of current reads".to_owned()];
    if let Some(gates) = adoption.get("gates").and_then(Value::as_object) {
        for (gate, result) in gates {
            let Some(text) = result.as_str() else {
                continue;
            };
            if text == "pass" {
                continue;
            }
            let detail = text.strip_prefix("fail: ").unwrap_or(text);
            lines.push(format!("  {gate}: {detail}"));
        }
    }
    lines.push("  see: wi work list".to_owned());
    if let Some(work) = adoption.get("work").and_then(Value::as_array) {
        lines.extend(
            work.iter()
                .filter_map(Value::as_str)
                .map(|path| format!("  {path}")),
        );
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn candidate_lists_failed_gates_and_work() {
        let value = json!({"adoption":{"outcome":"candidate","gates":{
            "classification":"fail: null kind, summary",
            "sources":"fail: sources unresolved",
            "conflict":"pass"},"work":["work/gate-x.md"]}});
        let text = explain(Some(&value)).unwrap();
        assert_eq!(
            text,
            "candidate: the engine kept this claim out of current reads\n  classification: null kind, summary\n  sources: sources unresolved\n  see: wi work list\n  work/gate-x.md"
        );
    }

    #[test]
    fn deferred_and_rejected_have_one_line() {
        let deferred = json!({"adoption":{"outcome":"deferred"}});
        assert_eq!(
            explain(Some(&deferred)).unwrap(),
            "deferred: change cap reached; it resumes on its own"
        );
        let rejected = json!({"adoption":{"outcome":"rejected","reason":"locked"}});
        assert_eq!(explain(Some(&rejected)).unwrap(), "rejected: locked");
    }

    #[test]
    fn active_unchanged_and_plain_writes_are_silent() {
        assert!(explain(Some(&json!({"adoption":{"outcome":"active"}}))).is_none());
        assert!(explain(Some(&json!({"adoption":{"outcome":"unchanged"}}))).is_none());
        assert!(explain(Some(&json!({"fork":{}}))).is_none());
        assert!(explain(None).is_none());
    }
}
