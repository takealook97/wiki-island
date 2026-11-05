//! Note builders for lifecycle tests. Notes are rendered as real Markdown
//! and parsed by `wi-note`, so the tests exercise the same view the host has.
#![allow(dead_code)]

use chrono::{TimeDelta, TimeZone, Utc};
use serde_json::{Map, Value, json};
use wi_lifecycle::{NoteSnapshot, Timestamp, format_time, sha256};

pub fn day(n: i64) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + TimeDelta::days(n)
}

pub fn at(n_days: i64, seconds: i64) -> Timestamp {
    day(n_days) + TimeDelta::seconds(seconds)
}

pub fn ts(t: Timestamp) -> Value {
    json!(format_time(t))
}

/// A source pin over `bytes`, optionally citing `span`.
pub fn pin(reference: &str, bytes: &[u8], span: Option<(usize, usize)>) -> Value {
    let mut pin = json!({
        "ref": reference,
        "commit": null,
        "pending": true,
        "source_hash": sha256(bytes),
    });
    if let Some((start, end)) = span {
        pin["span_hash"] = json!(sha256(&bytes[start..end]));
        pin["locator"] = json!({"span": {"start": start, "end": end}});
    }
    pin
}

pub fn url_pin(reference: &str, bytes: &[u8], url: &str) -> Value {
    json!({
        "ref": reference,
        "commit": null,
        "pending": true,
        "source_hash": sha256(bytes),
        "span_hash": sha256(bytes),
        "locator": {"span": {"start": 0, "end": bytes.len()}, "url": url},
    })
}

#[derive(Clone)]
pub struct Claim {
    pub path: String,
    fields: Map<String, Value>,
    body: String,
}

impl Claim {
    /// An active-ready user `fact` about `svc:payments` in scope `work`.
    pub fn fact(path: &str) -> Self {
        let fields = json!({
            "type": "memory",
            "kind": "fact",
            "claim_kind": "current-state",
            "status": "candidate",
            "summary": format!("claim {path}"),
            "entities": [{"id": "svc:payments", "label": "Payments", "source": null}],
            "scope": "work",
            "applicability": {"from": null, "until": null, "conditions": [], "supersedes": []},
            "observed_at": format_time(day(0)),
            "origin": {"kind": "user", "locator": "file", "captured_at": format_time(day(0))},
            "sources": [],
            "conflict_with": [],
        });
        Self {
            path: path.into(),
            fields: fields.as_object().cloned().unwrap(),
            body: format!("Body of {path}."),
        }
    }

    pub fn rule(path: &str) -> Self {
        Self::fact(path)
            .set("kind", json!("rule"))
            .set("claim_kind", json!("intent"))
    }

    pub fn set(self, key: &str, value: Value) -> Self {
        let mut fields = self.fields;
        fields.insert(key.into(), value);
        Self { fields, ..self }
    }

    pub fn remove(self, key: &str) -> Self {
        let mut fields = self.fields;
        fields.remove(key);
        Self { fields, ..self }
    }

    pub fn at_path(self, path: &str) -> Self {
        Self {
            path: path.into(),
            ..self
        }
    }

    pub fn body(self, body: &str) -> Self {
        Self {
            body: body.into(),
            ..self
        }
    }

    pub fn active(self) -> Self {
        self.set("status", json!("active"))
    }

    pub fn agent(self) -> Self {
        self.set(
            "origin",
            json!({"kind": "agent", "locator": "mcp:wi_write", "captured_at": format_time(day(0))}),
        )
    }

    pub fn observed(self, t: Timestamp) -> Self {
        self.set("observed_at", ts(t))
    }

    pub fn slot(self, slot: &str, value: Value) -> Self {
        self.set("slot", json!(slot)).set("value", value)
    }

    pub fn sources(self, pins: Vec<Value>) -> Self {
        self.set("sources", Value::Array(pins))
    }

    pub fn supersedes(self, targets: &[&str]) -> Self {
        let mut app = self.fields["applicability"].clone();
        app["supersedes"] = json!(targets);
        self.set("applicability", app)
    }

    pub fn applicability(self, key: &str, value: Value) -> Self {
        let mut app = self.fields["applicability"].clone();
        app[key] = value;
        self.set("applicability", app)
    }

    pub fn text(&self) -> String {
        let mut out = String::from("---\n");
        for (key, value) in &self.fields {
            out.push_str(&format!("{key}: {value}\n"));
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out.push('\n');
        out
    }

    pub fn note(&self) -> wi_note::Note {
        wi_note::Note::parse(self.text().as_bytes()).unwrap()
    }

    pub fn snap(&self) -> NoteSnapshot {
        let snap = NoteSnapshot::from_note(&self.path, &self.note());
        assert!(snap.unparseable.is_none(), "{:?}", snap.unparseable);
        snap
    }
}

pub fn plain(path: &str, text: &str) -> NoteSnapshot {
    NoteSnapshot::from_bytes(path, text.as_bytes())
}
