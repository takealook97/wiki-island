//! Density ladder for delivery. When a complete note does not fit the result
//! budget, a plain note can be presented at a computed, smaller tier instead
//! of being withheld: `core` (complete structural or qualified blocks), `summary` (the
//! owner's frontmatter `summary`, else the first sentence) or `title` (no
//! body). Tiers are computed at delivery time and never stored.
//!
//! Lifecycle claims (`lifecycle != "none"`) are never reduced: a claim and its
//! qualifiers are delivered whole or withheld whole. Metadata, sources and
//! source spans are never touched by any tier; only `content` shrinks.

use serde_json::{Value, json};

/// Longest computed first-sentence summary, in characters.
const SUMMARY_MAX_CHARS: usize = 200;

/// Every presentation of `hit`, richest first. The first entry is the hit as
/// given (`presented: full`); later entries are strictly smaller tiers.
pub(crate) fn tiers(mut hit: Value) -> Vec<Value> {
    hit["presented"] = json!("full");
    let reducible =
        hit["lifecycle"] == "none" && hit["content"].as_str().is_some_and(|c| !c.is_empty());
    if !reducible {
        return vec![hit];
    }
    let content = hit["content"].as_str().unwrap_or_default().to_owned();
    let body = body_of(&content);
    let summary = hit["metadata"]["summary"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| first_sentence(body));
    let summary = summary.unwrap_or_default();
    // A core made of headings only, or no larger than the summary, adds no
    // step between full and summary.
    let core = core(body)
        .filter(|c| c.lines().any(|l| !heading(l.trim_start())) && c.len() > summary.len())
        .unwrap_or_default();
    let mut out = vec![hit.clone()];
    let mut last = content.len();
    for (name, text) in [
        ("core", core),
        ("summary", summary),
        ("title", String::new()),
    ] {
        if text.len() >= last || (name != "title" && text.is_empty()) {
            continue;
        }
        last = text.len();
        let mut reduced = hit.clone();
        reduced["content"] = json!(text);
        reduced["presented"] = json!(name);
        out.push(reduced);
    }
    out
}

/// Body text without a leading frontmatter block. Metadata already carries
/// the frontmatter fields, so reduced tiers do not repeat them.
fn body_of(content: &str) -> &str {
    match wi_note::Note::parse(content.as_bytes()) {
        Ok(note) => {
            let start = note.body_start().min(content.len());
            content.get(start..).unwrap_or(content)
        }
        Err(_) => content,
    }
}

/// Words that narrow or reverse what a nearby statement says. A block that
/// carries one is never dropped from the core tier.
const QUALIFIER_WORDS: [&str; 22] = [
    "except",
    "excepting",
    "excluding",
    "unless",
    "only",
    "never",
    "not",
    "no",
    "until",
    "without",
    "however",
    "but",
    "if",
    "when",
    "whenever",
    "provided",
    "otherwise",
    "before",
    "after",
    "while",
    "instead",
    "neither",
];
/// Korean qualifier fragments (exception, condition, restriction, negation).
const QUALIFIER_FRAGMENTS: [&str; 14] = [
    "제외", "단,", "단 ", "경우", "않", "금지", "말고", "외에", "에만", "뿐", "까지", "아니",
    "때만", "대신",
];

/// Complete Markdown blocks, each kept whole and in order: headings, list
/// items whose text holds a numbered step, categorized observation
/// (`- [rule] ...`) or bold-led line anywhere (the item line plus every
/// continuation, nested and lazy line), bold-led paragraphs, and every list
/// item or paragraph that carries a qualifier. Top-level code fences are
/// skipped. `None` when the body cannot be split into blocks without
/// guessing (an unterminated code fence); the caller then skips the tier.
pub(crate) fn core(body: &str) -> Option<String> {
    let lines: Vec<&str> = body.lines().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim().is_empty() {
            i += 1;
            continue;
        }
        let (end, kind) = block(&lines, i)?;
        let text = &lines[i..end];
        let keep = match kind {
            Block::Heading => true,
            Block::Fence => false,
            Block::Item | Block::Paragraph => {
                text.iter().any(|l| structural(l.trim_start())) || qualified(text)
            }
        };
        if keep {
            for line in text {
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
        i = end;
    }
    Some(out)
}

#[derive(Clone, Copy)]
enum Block {
    Heading,
    Fence,
    Item,
    Paragraph,
}

fn structural(trimmed: &str) -> bool {
    numbered(trimmed) || observation(trimmed) || trimmed.starts_with("**")
}

fn qualified(text: &[&str]) -> bool {
    text.iter().any(|line| {
        let lower = line.to_lowercase();
        lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| QUALIFIER_WORDS.contains(&w))
            || QUALIFIER_FRAGMENTS.iter().any(|f| lower.contains(f))
    })
}

/// Leading indentation in columns (a tab counts as four).
fn indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// A list item opener: its indentation and the column its content starts at.
fn item_start(line: &str) -> Option<(usize, usize)> {
    let at = indent(line);
    let trimmed = line.trim_start();
    let marker = if trimmed.starts_with(['-', '*', '+']) {
        1
    } else {
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        if !(1..=9).contains(&digits) || !trimmed[digits..].starts_with(['.', ')']) {
            return None;
        }
        digits + 1
    };
    let rest = &trimmed[marker..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let gap = indent(rest);
    let gap = if gap == 0 || gap > 4 { 1 } else { gap };
    Some((at, at + marker + gap))
}

/// A fence opener: its marker character and run length.
fn fence_open(trimmed: &str) -> Option<(char, usize)> {
    let c = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = trimmed.chars().take_while(|x| *x == c).count();
    (run >= 3).then_some((c, run))
}

/// Index just past the fence that opens at `start`, if it closes.
fn fence_end(lines: &[&str], start: usize, (c, run): (char, usize)) -> Option<usize> {
    (start + 1..lines.len())
        .find(|&j| {
            let t = lines[j].trim();
            t.chars().take_while(|x| *x == c).count() >= run && t.chars().all(|x| x == c)
        })
        .map(|j| j + 1)
}

/// A line that begins a new block and so ends a paragraph or lazy line.
fn interrupts(line: &str) -> bool {
    let trimmed = line.trim_start();
    heading(trimmed) || fence_open(trimmed).is_some() || item_start(line).is_some()
}

/// The block starting at the non-blank line `start`: its end index and kind.
fn block(lines: &[&str], start: usize) -> Option<(usize, Block)> {
    let line = lines[start];
    let trimmed = line.trim_start();
    if let Some(fence) = fence_open(trimmed) {
        return Some((fence_end(lines, start, fence)?, Block::Fence));
    }
    if heading(trimmed) {
        return Some((start + 1, Block::Heading));
    }
    let Some((_, content)) = item_start(line) else {
        let mut j = start + 1;
        while j < lines.len() && !lines[j].trim().is_empty() && !interrupts(lines[j]) {
            j += 1;
        }
        return Some((j, Block::Paragraph));
    };
    let mut j = start + 1;
    while j < lines.len() {
        let next = lines[j];
        if next.trim().is_empty() {
            // Blank lines stay inside the item only when indented content follows.
            let Some(k) = (j..lines.len()).find(|&k| !lines[k].trim().is_empty()) else {
                break;
            };
            if indent(lines[k]) < content {
                break;
            }
            j = k;
            continue;
        }
        if indent(next) >= content {
            j = match fence_open(next.trim_start()) {
                Some(fence) => fence_end(lines, j, fence)?,
                None => j + 1,
            };
            continue;
        }
        if interrupts(next) {
            break;
        }
        // Lazy continuation of the item's paragraph.
        j += 1;
    }
    Some((j, Block::Item))
}

fn heading(line: &str) -> bool {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    (1..=6).contains(&hashes) && line[hashes..].starts_with([' ', '\t'])
}

fn numbered(line: &str) -> bool {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    (1..=3).contains(&digits)
        && line[digits..].starts_with(['.', ')'])
        && line[digits + 1..].starts_with([' ', '\t'])
}

fn observation(line: &str) -> bool {
    let Some(rest) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
    else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with('[')
        && rest
            .find(']')
            .is_some_and(|end| end > 1 && rest[end + 1..].starts_with([' ', '\t']))
}

/// The first sentence of the first meaningful body line, list and heading
/// markers removed. Longer than 200 characters: cut at a character boundary
/// and marked with an ellipsis.
pub(crate) fn first_sentence(body: &str) -> Option<String> {
    let mut fence = false;
    let line = body.lines().find_map(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = !fence;
            return None;
        }
        let text = strip_markers(trimmed);
        (!fence && !text.is_empty()).then_some(text)
    })?;
    let mut end = line.len();
    for (i, c) in line.char_indices() {
        let next = i + c.len_utf8();
        let terminal = match c {
            '.' | '?' | '!' => line[next..].starts_with(char::is_whitespace),
            '。' | '？' | '！' => true,
            _ => false,
        };
        if terminal {
            end = next;
            break;
        }
    }
    let sentence = line[..end].trim();
    if sentence.chars().count() <= SUMMARY_MAX_CHARS {
        return Some(sentence.to_owned());
    }
    let cut: String = sentence.chars().take(SUMMARY_MAX_CHARS).collect();
    Some(format!("{}…", cut.trim_end()))
}

fn strip_markers(line: &str) -> &str {
    let mut rest = line.trim();
    loop {
        let before = rest;
        let hashes = rest.bytes().take_while(|b| *b == b'#').count();
        if (1..=6).contains(&hashes) && rest[hashes..].starts_with([' ', '\t']) {
            rest = rest[hashes..].trim_start();
        }
        for marker in ["- ", "* ", "+ ", "> "] {
            if let Some(stripped) = rest.strip_prefix(marker) {
                rest = stripped.trim_start();
            }
        }
        if numbered(rest) {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            rest = rest[digits + 1..].trim_start();
        }
        if rest == before {
            return rest;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(content: &str) -> Value {
        json!({"path":"a.md","lifecycle":"none","content":content,"metadata":{}})
    }

    #[test]
    fn core_keeps_structural_lines_whole() {
        let body = "Intro prose that is long.\n# Plan\n1. Step one, only on weekdays\nfiller\n- [rule] Never deploy on Friday (EU only)\n```\n# not a heading\n```\n**Must** keep this\n";
        // `filler` is a lazy continuation of step one, so it stays with it.
        assert_eq!(
            core(body).unwrap(),
            "# Plan\n1. Step one, only on weekdays\nfiller\n- [rule] Never deploy on Friday (EU only)\n**Must** keep this\n"
        );
    }

    #[test]
    fn core_keeps_continuation_lines_that_qualify_a_step() {
        let body = "Payment procedure.\n# Rules\n1. Always deploy the payment change.\n   Except on Friday: never deploy the payment change.\n2. Record the payment outcome.\n\nBackground payment details.\n";
        assert_eq!(
            core(body).unwrap(),
            "# Rules\n1. Always deploy the payment change.\n   Except on Friday: never deploy the payment change.\n2. Record the payment outcome.\n"
        );
    }

    #[test]
    fn core_keeps_nested_lists_and_qualified_blocks_whole() {
        let body = "Deploy notes.\n\n- Release train\n  1. Build the image\n  2. Ship it\n     - after review\n\n  Rollback is manual.\n- Plain remark\n\nThis applies only in the EU.\n\nUnrelated prose.\n";
        assert_eq!(
            core(body).unwrap(),
            "- Release train\n  1. Build the image\n  2. Ship it\n     - after review\n\n  Rollback is manual.\nThis applies only in the EU.\n"
        );
    }

    #[test]
    fn core_keeps_korean_qualifiers_with_their_step() {
        let body = "배포 안내입니다.\n# 규칙\n1. 결제 변경을 배포한다.\n   금요일은 제외한다.\n2. 결과를 기록한다.\n\n- 주말에는 배포하지 않는다.\n\n참고 자료 모음.\n";
        let core = core(body).unwrap();
        assert_eq!(
            core,
            "# 규칙\n1. 결제 변경을 배포한다.\n   금요일은 제외한다.\n2. 결과를 기록한다.\n- 주말에는 배포하지 않는다.\n"
        );
    }

    #[test]
    fn an_unterminated_fence_skips_the_core_tier() {
        let body = "1. Step\n```\ncode without an end\n";
        assert_eq!(core(body), None);
        let content = format!("Lead sentence. More.\n{body}{}", "word ".repeat(100));
        let names: Vec<_> = tiers(plain(&content))
            .iter()
            .map(|t| t["presented"].clone())
            .collect();
        assert_eq!(names, vec!["full", "summary", "title"]);
    }

    #[test]
    fn first_sentence_cuts_at_a_terminator_or_marks_the_cut() {
        assert_eq!(
            first_sentence("\n## 1. Ship it. Then rest.").as_deref(),
            Some("Ship it.")
        );
        assert_eq!(
            first_sentence("v1.2 is out").as_deref(),
            Some("v1.2 is out")
        );
        assert_eq!(
            first_sentence("배포한다。다음").as_deref(),
            Some("배포한다。")
        );
        let long = "가".repeat(300);
        let cut = first_sentence(&long).unwrap();
        assert_eq!(cut.chars().count(), SUMMARY_MAX_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn claims_have_one_tier_and_plain_notes_shrink_strictly() {
        let claim = json!({"lifecycle":"memory","content":"x".repeat(100),"metadata":{}});
        let tiers_of_claim = tiers(claim);
        assert_eq!(tiers_of_claim.len(), 1);
        assert_eq!(tiers_of_claim[0]["presented"], "full");

        let content = format!(
            "Lead sentence. More.\n# H\n1. First step, EU only\n2. Second step\n\n{}",
            "word ".repeat(100)
        );
        let all = tiers(plain(&content));
        let names: Vec<_> = all.iter().map(|t| t["presented"].clone()).collect();
        assert_eq!(names, vec!["full", "core", "summary", "title"]);
        let lens: Vec<_> = all
            .iter()
            .map(|t| t["content"].as_str().unwrap().len())
            .collect();
        assert!(lens.windows(2).all(|w| w[0] > w[1]), "{lens:?}");
        assert_eq!(all[2]["content"], "Lead sentence.");
    }

    #[test]
    fn frontmatter_summary_wins_and_metadata_is_untouched() {
        let mut hit = plain(&format!(
            "---\nsummary: Owner text\n---\n{}",
            "body ".repeat(50)
        ));
        hit["metadata"] = json!({"summary":"Owner text","scope":"EU only"});
        let all = tiers(hit);
        let summary = all.iter().find(|t| t["presented"] == "summary").unwrap();
        assert_eq!(summary["content"], "Owner text");
        assert!(all.iter().all(|t| t["metadata"]["scope"] == "EU only"));
    }
}
