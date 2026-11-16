//! Capture and proposal filters. Model-free and conservative: a match
//! drops the text with a reason; no match proves nothing about the text.
//!
//! Not detectable without a model, and therefore the host's duty: assistant
//! suggestions the user did not adopt, and widening a task-specific
//! instruction into a general preference.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

use crate::model::sha256;

/// Secret patterns: (name, regex). Matched anywhere in the text.
pub const SECRET_PATTERNS: &[(&str, &str)] = &[
    ("private-key", r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----"),
    ("aws-access-key-id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
    (
        "github-token",
        r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})\b",
    ),
    ("slack-token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}"),
    ("api-secret-key", r"\bsk-[A-Za-z0-9_-]{20,}"),
    ("stripe-key", r"\b(?:sk|rk)_live_[A-Za-z0-9]{16,}"),
    ("google-api-key", r"\bAIza[0-9A-Za-z_-]{35}\b"),
    (
        "jwt",
        r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
    ),
    (
        "credential-assignment",
        r#"(?i)\b(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret)\b\s*[:=]\s*["']?([^\s"']+)"#,
    ),
    (
        "credential-url",
        r"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s:/@]+:[^\s/@]+@",
    ),
];

/// One-off value heuristics: verification codes and bare values with no words.
pub const ONE_OFF_PATTERNS: &[(&str, &str)] = &[
    (
        "verification-code",
        r"(?i)\b(?:otp|one[- ]time (?:code|password|passcode)|verification code|2fa code|security code|login code)\b\D{0,20}\b\d{4,8}\b",
    ),
    (
        "bare-value",
        r"^\s*(?:[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}|[0-9a-fA-F]{16,}|[+-]?\d[\d,._-]*)\s*$",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case", tag = "reason", content = "pattern")]
pub enum FilterReason {
    Secret(String),
    OneOffValue(String),
    /// The body hash equals an existing note's.
    Duplicate(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "outcome", content = "reasons")]
pub enum FilterOutcome {
    Accept,
    /// Nothing to store; valid when a `no_memory_reason` is given.
    Empty,
    Drop(Vec<FilterReason>),
}

/// Values that stand in for a credential rather than being one:
/// `<...>`, `${...}`, a run of `*` and `redacted`.
fn is_placeholder(value: &str) -> bool {
    let bare = value.trim_matches(|c: char| matches!(c, '[' | ']' | '(' | ')' | ',' | ';'));
    (bare.starts_with('<') && bare.ends_with('>'))
        || (bare.starts_with("${") && bare.ends_with('}'))
        || (bare.len() >= 3 && bare.chars().all(|c| c == '*'))
        || bare.eq_ignore_ascii_case("redacted")
}

/// A pattern with a capture group matches only where the captured value is
/// not a placeholder; any other pattern matches anywhere.
fn secret_matches(re: &Regex, text: &str) -> bool {
    if re.captures_len() < 2 {
        return re.is_match(text);
    }
    re.captures_iter(text)
        .filter_map(|c| c.get(1))
        .any(|value| !is_placeholder(value.as_str()))
}

fn compiled(patterns: &'static [(&'static str, &'static str)]) -> Vec<(&'static str, Regex)> {
    patterns
        .iter()
        .filter_map(|(name, pattern)| Regex::new(pattern).ok().map(|re| (*name, re)))
        .collect()
}

fn secrets() -> &'static [(&'static str, Regex)] {
    static CELL: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    CELL.get_or_init(|| compiled(SECRET_PATTERNS))
}

fn one_offs() -> &'static [(&'static str, Regex)] {
    static CELL: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    CELL.get_or_init(|| compiled(ONE_OFF_PATTERNS))
}

/// A whole private key block (or the rest of the text when it is cut off).
fn private_key_block() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        Regex::new(
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
        )
        .expect("static private key pattern")
    })
}

/// Replacement text for redacted secrets.
pub const REDACTED: &str = "[redacted]";

/// Replace every secret the capture filter would drop with `[redacted]` and
/// return the new text with the number of replacements. A pattern with a
/// capture group replaces the captured value only, and placeholders such as
/// `<token>` or `[redacted]` stay as they are, so the result is stable.
pub fn redact_secrets(text: &str) -> (String, usize) {
    let mut count = 0;
    let block = private_key_block();
    let mut out = if block.is_match(text) {
        count += block.find_iter(text).count();
        block.replace_all(text, REDACTED).into_owned()
    } else {
        text.to_owned()
    };
    for (_, re) in secrets() {
        let mut next = String::with_capacity(out.len());
        let mut last = 0;
        for caps in re.captures_iter(&out) {
            let target = if re.captures_len() < 2 {
                caps.get(0)
            } else {
                caps.get(1).filter(|v| !is_placeholder(v.as_str()))
            };
            let Some(found) = target else { continue };
            next.push_str(&out[last..found.start()]);
            next.push_str(REDACTED);
            last = found.end();
            count += 1;
        }
        next.push_str(&out[last..]);
        out = next;
    }
    (out, count)
}

/// Filter capture or proposal text. `existing_body_hashes` are the body
/// hashes of existing notes (trimmed body, as in `NoteSnapshot::body_hash`).
pub fn capture_filter(text: &str, existing_body_hashes: &BTreeSet<String>) -> FilterOutcome {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return FilterOutcome::Empty;
    }
    let mut reasons: Vec<FilterReason> = secrets()
        .iter()
        .filter(|(_, re)| secret_matches(re, trimmed))
        .map(|(name, _)| FilterReason::Secret((*name).into()))
        .chain(
            one_offs()
                .iter()
                .filter(|(_, re)| re.is_match(trimmed))
                .map(|(name, _)| FilterReason::OneOffValue((*name).into())),
        )
        .collect();
    let hash = sha256(trimmed.as_bytes());
    if existing_body_hashes.contains(&hash) {
        reasons.push(FilterReason::Duplicate(hash));
    }
    if reasons.is_empty() {
        FilterOutcome::Accept
    } else {
        reasons.sort();
        FilterOutcome::Drop(reasons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pattern_compiles() {
        assert_eq!(secrets().len(), SECRET_PATTERNS.len());
        assert_eq!(one_offs().len(), ONE_OFF_PATTERNS.len());
    }

    #[test]
    fn secrets_and_one_offs_are_dropped_and_prose_is_kept() {
        let none = BTreeSet::new();
        let fake_aws = format!("key {}{}", "AKIA", "ABCDEFGHIJKLMNOP");
        for text in [
            fake_aws.as_str(),
            "password = hunter2hunter2",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "postgres://admin:pw123@db.internal/app",
            "Your verification code is 483920",
            "  12345678  ",
            "3f2a9c0d1e2b4a5f",
        ] {
            assert!(
                matches!(capture_filter(text, &none), FilterOutcome::Drop(_)),
                "{text}"
            );
        }
        for text in [
            "The payment service listens on port 443.",
            "결제 후 7일 안에 환불",
            "Use the password manager for shared accounts.",
        ] {
            assert_eq!(capture_filter(text, &none), FilterOutcome::Accept, "{text}");
        }
        assert_eq!(capture_filter("   ", &none), FilterOutcome::Empty);
    }

    #[test]
    fn short_credential_assignments_drop_and_placeholders_pass() {
        let none = BTreeSet::new();
        for text in [
            "password: abc",
            "password: hunter2",
            "api_key: abc123",
            "token=x1",
            "client_secret = \"s3\"",
        ] {
            assert_eq!(
                capture_filter(text, &none),
                FilterOutcome::Drop(vec![FilterReason::Secret("credential-assignment".into())]),
                "{text}"
            );
        }
        for text in [
            "password = <redacted>",
            "api_key: ${API_KEY}",
            "password: ***",
            "secret: redacted",
            "token: [REDACTED]",
        ] {
            assert_eq!(capture_filter(text, &none), FilterOutcome::Accept, "{text}");
        }
    }

    #[test]
    fn redaction_replaces_secrets_and_keeps_context_and_placeholders() {
        let fake_aws = format!("{}{}", "AKIA", "ABCDEFGHIJKLMNOP");
        let text = format!("key {fake_aws} and password: hunter2 and token: <token> done");
        let (out, n) = redact_secrets(&text);
        assert_eq!(
            out,
            "key [redacted] and password: [redacted] and token: <token> done"
        );
        assert_eq!(n, 2);
        let (again, n) = redact_secrets(&out);
        assert_eq!((again.as_str(), n), (out.as_str(), 0));
        let (out, n) = redact_secrets("postgres://admin:pw123@db.internal/app");
        assert_eq!((out.as_str(), n), ("[redacted]db.internal/app", 1));
        let (out, n) = redact_secrets(
            "before\n-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----\nafter",
        );
        assert_eq!((out.as_str(), n), ("before\n[redacted]\nafter", 1));
        let (out, n) = redact_secrets("-----BEGIN PRIVATE KEY-----\ncut off");
        assert_eq!((out.as_str(), n), ("[redacted]", 1));
        assert_eq!(
            redact_secrets("결제 후 7일 안에 환불"),
            ("결제 후 7일 안에 환불".to_string(), 0)
        );
    }

    #[test]
    fn duplicate_body_hash_is_dropped() {
        let existing: BTreeSet<String> = [sha256(b"Port is 443.")].into();
        assert!(matches!(
            capture_filter("  Port is 443.\n", &existing),
            FilterOutcome::Drop(r) if matches!(r[0], FilterReason::Duplicate(_))
        ));
    }
}
