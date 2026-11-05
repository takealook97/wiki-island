use crate::{Error, Query, Result, SearchMode, script::analyze_script_query};

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A small, safe query grammar: terms, quoted phrases and binary AND/OR/NOT.
/// Column selectors and raw FTS expressions are never interpolated from input.
fn words(text: &str, prefix: bool) -> Result<String> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '"' {
            let mut term = String::new();
            let mut closed = false;
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        term.push('"');
                    } else {
                        closed = true;
                        break;
                    }
                } else {
                    term.push(c);
                }
            }
            if !closed || term.is_empty() {
                return Err(Error::InvalidQuery("unclosed or empty phrase".into()));
            }
            tokens.push((
                format!("{}{}", quote(&term), if prefix { "*" } else { "" }),
                false,
            ));
        } else {
            let mut term = c.to_string();
            while chars.peek().is_some_and(|c| !c.is_whitespace()) {
                if let Some(c) = chars.next() {
                    term.push(c);
                }
            }
            let operator = ["AND", "OR", "NOT"].contains(&term.as_str());
            if operator {
                tokens.push((term, true));
            } else {
                let caller_star = term.ends_with('*');
                let term = term.trim_end_matches('*');
                let star =
                    (prefix || caller_star) && !(term.contains('/') && term.ends_with(".md"));
                if text.split_whitespace().count() > 1 && !term.chars().any(char::is_alphanumeric) {
                    continue;
                }
                if term.is_empty() {
                    return Err(Error::InvalidQuery("wildcard must follow a term".into()));
                }
                tokens.push((
                    format!("{}{}", quote(term), if star { "*" } else { "" }),
                    false,
                ));
            }
        }
    }
    let has_ops = tokens.iter().any(|(_, op)| *op);
    let mut result = String::new();
    let mut expect_term = true;
    for (term, operator) in tokens {
        if operator {
            if expect_term {
                return Err(Error::InvalidQuery(
                    "Boolean operator requires a left operand".into(),
                ));
            }
            result.push(' ');
            result.push_str(&term);
            result.push(' ');
            expect_term = true;
        } else {
            if !expect_term {
                result.push_str(" AND ");
            }
            // Explicit Boolean queries use exact terms.
            result.push_str(if has_ops {
                term.trim_end_matches('*')
            } else {
                &term
            });
            expect_term = false;
        }
    }
    if expect_term {
        return Err(Error::InvalidQuery("query requires a term".into()));
    }
    Ok(result)
}

pub(crate) fn expression(query: &Query) -> Result<Option<String>> {
    let text = query.text.trim();
    if text.is_empty() || text == "*" || query.mode == SearchMode::Permalink {
        return Ok(None);
    }
    if query.mode == SearchMode::Title {
        return Ok(Some(format!("title: ({})", words(text, false)?)));
    }
    let analyzed = analyze_script_query(text);
    let mut clauses = Vec::new();
    if let Some(word_text) = analyzed.word_text {
        let prepared = words(&word_text, true)?;
        if word_text.split_whitespace().any(|s| s == "NOT") {
            clauses.push(format!("{{title content}}: ({prepared})"));
        } else {
            clauses.push(format!("(title: ({prepared}) OR content: ({prepared}))"));
        }
    }
    if !analyzed.gram_phrases.is_empty() {
        clauses.push(format!(
            "script_ngrams: ({})",
            analyzed
                .gram_phrases
                .iter()
                .map(|p| quote(&p.join(" ")))
                .collect::<Vec<_>>()
                .join(" AND ")
        ));
    }
    Ok(Some(clauses.join(" AND ")))
}

/// Ranked recall expression for natural-language queries: OR over every
/// word term and every script n-gram, scored by bm25. It is used only after
/// the strict (all-terms) expression, so all-terms hits always rank first.
/// Single-term queries, explicit Boolean syntax and quoted phrases keep the
/// strict behavior and return `None`.
pub(crate) fn ranked_expression(query: &Query) -> Option<String> {
    let text = query.text.trim();
    if query.mode != SearchMode::Text || text.split_whitespace().count() < 2 {
        return None;
    }
    let explicit = text.contains('"')
        || text
            .split_whitespace()
            .any(|t| ["AND", "OR", "NOT"].contains(&t));
    if explicit {
        return None;
    }
    let analyzed = analyze_script_query(text);
    let mut terms: Vec<String> = Vec::new();
    let mut push = |term: String| {
        if !terms.contains(&term) {
            terms.push(term);
        }
    };
    if let Some(words) = &analyzed.word_text {
        for word in words.split_whitespace() {
            let word = word.trim_end_matches('*');
            if !word.chars().any(char::is_alphanumeric) {
                continue;
            }
            push(format!("{{title content}}: {}*", quote(word)));
        }
    }
    for gram in analyzed.gram_phrases.iter().flatten() {
        push(format!("script_ngrams: {}", quote(gram)));
    }
    (terms.len() >= 2).then(|| terms.join(" OR "))
}

pub(crate) fn star_match(pattern: &str, value: &str) -> bool {
    // Only '*' is a wildcard, so authored brackets and question marks stay literal.
    let (p, v) = (pattern.as_bytes(), value.as_bytes());
    let (mut i, mut j, mut star, mut retry) = (0, 0, None, 0);
    while j < v.len() {
        if i < p.len() && p[i] == v[j] {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == b'*' {
            star = Some(i);
            i += 1;
            retry = j;
        } else if let Some(s) = star {
            retry += 1;
            j = retry;
            i = s + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}

pub(crate) fn sqlite_glob(pattern: &str) -> String {
    pattern
        .chars()
        .map(|c| match c {
            '[' => "[[]".into(),
            '?' => "[?]".into(),
            _ => c.to_string(),
        })
        .collect()
}
