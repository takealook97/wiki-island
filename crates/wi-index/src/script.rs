//! Unicode script runs and n-grams for portable substring search.
use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_normalization::UnicodeNormalization;

pub fn is_script(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11ff | 0x0e00..=0x0eff | 0x0f00..=0x0fff |
        0x1000..=0x109f | 0x1780..=0x17ff | 0x19e0..=0x19ff |
        0x3005..=0x3007 | 0x3021..=0x3029 | 0x3031..=0x3035 |
        0x3038..=0x303b | 0x3040..=0x30ff | 0x3100..=0x318f |
        0x31a0..=0x31bf | 0x31f0..=0x31ff | 0x3400..=0x4dbf |
        0x4e00..=0x9fff | 0xa960..=0xa97f | 0xa9e0..=0xa9ff |
        0xaa60..=0xaa7f | 0xac00..=0xd7af | 0xd7b0..=0xd7ff |
        0xf900..=0xfaff | 0x1aff0..=0x1afff | 0x1b000..=0x1b16f |
        0x20000..=0x2ffff | 0x30000..=0x323af)
}

pub fn script_runs(text: &str) -> Vec<Vec<String>> {
    let mut runs = Vec::new();
    let mut run: Vec<String> = Vec::new();
    for c in text.nfkc() {
        let mark = matches!(
            get_general_category(c),
            GeneralCategory::NonspacingMark
                | GeneralCategory::SpacingMark
                | GeneralCategory::EnclosingMark
        );
        if !run.is_empty() && matches!(c, '\u{200c}' | '\u{200d}') {
            continue;
        }
        if !run.is_empty() && mark {
            if let Some(unit) = run.last_mut() {
                unit.push(c);
            }
        } else if is_script(c) {
            run.push(c.to_string());
        } else if !run.is_empty() {
            runs.push(std::mem::take(&mut run));
        }
    }
    if !run.is_empty() {
        runs.push(run);
    }
    runs
}

pub fn script_run_grams(run: &[String]) -> Vec<String> {
    if run.len() == 1 {
        return run.to_vec();
    }
    run.windows(2)
        .map(|w| format!("{}{}", w[0], w[1]))
        .collect()
}

pub fn build_script_ngrams(texts: &[&str]) -> String {
    texts
        .iter()
        .flat_map(|s| script_runs(s))
        .map(|run| {
            let mut terms = run.clone();
            if run.len() > 1 {
                terms.extend(script_run_grams(&run));
            }
            terms.join(" ")
        })
        .collect::<Vec<_>>()
        .join(" script_boundary ")
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScriptQuery {
    pub word_text: Option<String>,
    pub gram_phrases: Vec<Vec<String>>,
}

pub fn analyze_script_query(text: &str) -> ScriptQuery {
    let padded = format!(" {text} ");
    if text.contains('"')
        || ["AND", "OR", "NOT"]
            .iter()
            .any(|op| padded.contains(&format!(" {op} ")))
    {
        return ScriptQuery {
            word_text: Some(text.into()),
            gram_phrases: Vec::new(),
        };
    }
    let words = text
        .split_whitespace()
        .filter(|token| token.nfkc().any(|c| c.is_alphanumeric() && !is_script(c)))
        .map(|token| {
            if ["AND", "OR", "NOT"].contains(&token) {
                token.to_lowercase()
            } else {
                token.into()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let gram_phrases: Vec<_> = script_runs(text)
        .iter()
        .map(|r| script_run_grams(r))
        .collect();
    let word_text = if !words.is_empty() {
        Some(words)
    } else if gram_phrases.is_empty() {
        Some(text.into())
    } else {
        None
    };
    ScriptQuery {
        word_text,
        gram_phrases,
    }
}
