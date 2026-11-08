use std::collections::HashSet;
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PermalinkError {
    #[error("explicit permalink is already owned by another note: {0}")]
    ExplicitCollision(String),
    #[error("permalink is empty")]
    Empty,
}

fn cjk(c: char) -> bool {
    matches!(c,'\u{4e00}'..='\u{9fff}' | '\u{3000}'..='\u{303f}' | '\u{3400}'..='\u{4dbf}')
}

/// Generate a normalized path identity. Korean is
/// transliterated by Unidecode; the specified CJK ranges are retained.
pub fn generate_permalink(rel_path: &str) -> String {
    let path: String = rel_path.nfc().collect();
    // Collapse repeated separators and ./ while preserving the leading slash form.
    let prefix = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let segments: Vec<_> = path
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let path = format!("{prefix}{}", segments.join("/"));
    let path = if path.is_empty() {
        ".".to_string()
    } else {
        path
    };
    let filename = path.rsplit('/').next().unwrap_or_default();
    let extension = filename
        .rsplit_once('.')
        .filter(|(stem, _)| stem.chars().any(|c| c != '.'))
        .map(|(_, ext)| ext.to_ascii_lowercase());
    // Pin the observed Python 3.12 differences; other extensions retain the database fallback.
    let recognized = match extension.as_deref() {
        Some(
            "yaml" | "yml" | "toml" | "ini" | "cfg" | "jsx" | "dat" | "map" | "php" | "lua" | "hpp"
            | "cs" | "vb" | "pem" | "img" | "rpm" | "war",
        ) => false,
        Some("rst" | "a" | "o" | "pyc") => true,
        Some(ext) => mime_guess::from_ext(ext).first().is_some(),
        None => false,
    };
    let base = if recognized {
        path.rsplit_once('.')
            .map_or(path.as_str(), |(base, _)| base)
    } else {
        &path
    };
    let has_cjk = base
        .chars()
        .any(|c| cjk(c) || matches!(c, '\u{ff00}'..='\u{ffef}'));
    let mut transliterated = String::new();
    for c in base.chars() {
        if has_cjk && cjk(c) {
            transliterated.push(c);
        } else if has_cjk && matches!(c, '\u{ff00}'..='\u{ffef}') {
            continue;
        } else {
            let text = match c {
                'し' | 'シ' => "shi",
                'ち' | 'チ' => "chi",
                'つ' | 'ツ' | 'っ' | 'ッ' => "tsu",
                'ー' => "-",
                '・' => "*",
                'Є' => "Ie",
                'є' => "ie",
                '¼' => " 1/4",
                '½' => " 1/2",
                '¾' => " 3/4",
                'Ŋ' => "NG",
                'ŋ' => "ng",
                _ => unidecode::unidecode_char(c),
            };
            if text != "[?]" {
                transliterated.push_str(text);
            }
        }
    }
    let mut clean = String::new();
    let mut prev = None;
    for c in transliterated.chars() {
        if let Some(p) = prev
            && ((has_cjk
                && ((cjk(p) && c.is_ascii_alphanumeric())
                    || (p.is_ascii_alphanumeric() && cjk(c))))
                || ((p.is_ascii_lowercase() || p.is_ascii_digit()) && c.is_ascii_uppercase()))
        {
            clean.push('-');
        }
        prev = Some(c);
        let c = c.to_ascii_lowercase();
        if c == '\'' {
            continue;
        }
        let c = if c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || matches!(c, '/' | '-' | '.')
            || (has_cjk && cjk(c))
        {
            c
        } else {
            '-'
        };
        if c != '-' || !clean.ends_with('-') {
            clean.push(c);
        }
    }
    clean
        .split('/')
        .map(|s| s.trim_matches('-'))
        .collect::<Vec<_>>()
        .join("/")
}

/// `existing` contains other notes' permalinks, excluding the current note.
/// For moves, pass the note's current permalink as `explicit` to keep its identity.
pub fn resolve_permalink(
    explicit: Option<&str>,
    rel_path: &str,
    existing: &HashSet<String>,
) -> Result<String, PermalinkError> {
    if let Some(explicit) = explicit.filter(|s| !s.is_empty()) {
        return if existing.contains(explicit) {
            Err(PermalinkError::ExplicitCollision(explicit.into()))
        } else {
            Ok(explicit.into())
        };
    }
    let desired = generate_permalink(rel_path);
    if desired.is_empty() {
        return Err(PermalinkError::Empty);
    }
    let mut permalink = desired.clone();
    let mut suffix = 1usize;
    while existing.contains(&permalink) {
        permalink = format!("{desired}-{suffix}");
        suffix += 1;
    }
    Ok(permalink)
}
