use crate::Note;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use regex::Regex;
use serde::Serialize;
use std::ops::Range;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Observation {
    pub category: Option<String>,
    pub content: String,
    pub tags: Vec<String>,
    pub context: Option<String>,
    /// One-based, half-open lines in the full note.
    pub line_range: Range<usize>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Relation {
    pub rel_type: String,
    pub target: String,
    pub context: Option<String>,
    pub line_range: Range<usize>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WikiLink {
    pub target: String,
    pub alias: Option<String>,
    /// UTF-8 byte offsets in the full note, including brackets.
    pub byte_range: Range<usize>,
}

static CATEGORY: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^\[([^\[\]()]+)\]\s+(.+)").ok());
static TIMESTAMP: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"^\d{1,3}:\d{2}(?::\d{2})?(?:[.,]\d{1,3})?(?:\s+-\s+\d{1,3}:\d{2}(?::\d{2})?(?:[.,]\d{1,3})?)?$").ok()
});

#[derive(Debug, Clone, Default)]
pub(crate) struct Parsed {
    observations: Vec<Observation>,
    relations: Vec<Relation>,
    links: Vec<WikiLink>,
}

impl Note {
    pub fn observations(&self) -> Vec<Observation> {
        self.parsed.get_or_init(|| parse(self)).observations.clone()
    }
    pub fn relations(&self) -> Vec<Relation> {
        self.parsed.get_or_init(|| parse(self)).relations.clone()
    }
    pub fn wikilinks(&self) -> Vec<WikiLink> {
        self.parsed.get_or_init(|| parse(self)).links.clone()
    }
}

struct InlineBlock {
    range: Range<usize>,
    in_list: bool,
    quoted: bool,
    heading: bool,
}

fn parse(note: &Note) -> Parsed {
    let mut parsed = Parsed::default();
    if !note.semantics_enabled() {
        return parsed;
    }
    let full_body = note.body();
    let body = full_body.trim();
    let body_start = note.body_start + full_body.len() - full_body.trim_start().len();
    let lines = LineIndex::new(note.to_bytes(), body_start);
    let mut ordered_relations = Vec::new();
    let mut blocks = Vec::new();
    let mut codes = Vec::new();
    let mut lists = 0usize;
    let mut quotes = 0usize;
    let mut code_block = false;
    let mut current: Option<InlineBlock> = None;
    for (event, range) in Parser::new(body).into_offset_iter() {
        let inline = matches!(
            &event,
            Event::Text(_)
                | Event::Code(_)
                | Event::InlineHtml(_)
                | Event::SoftBreak
                | Event::HardBreak
                | Event::Start(
                    Tag::Emphasis
                        | Tag::Strong
                        | Tag::Strikethrough
                        | Tag::Link { .. }
                        | Tag::Image { .. }
                )
                | Event::End(
                    TagEnd::Emphasis
                        | TagEnd::Strong
                        | TagEnd::Strikethrough
                        | TagEnd::Link
                        | TagEnd::Image
                )
        );
        if inline && !code_block {
            if let Some(block) = current.as_mut() {
                block.range.start = block.range.start.min(range.start);
                block.range.end = block.range.end.max(range.end);
            } else {
                let mut range = range.clone();
                while range.start > 0 && body.as_bytes()[range.start - 1] == b'\\' {
                    range.start -= 1;
                }
                current = Some(InlineBlock {
                    range,
                    in_list: lists > 0,
                    quoted: quotes > 0,
                    heading: false,
                });
            }
        } else if let Some(block) = current.take() {
            blocks.push(block);
        }
        match event {
            Event::Start(Tag::Item) => lists += 1,
            Event::End(TagEnd::Item) => lists = lists.saturating_sub(1),
            Event::Start(Tag::BlockQuote(_)) => quotes += 1,
            Event::End(TagEnd::BlockQuote(_)) => quotes = quotes.saturating_sub(1),
            Event::Start(Tag::CodeBlock(_)) => code_block = true,
            Event::End(TagEnd::CodeBlock) => code_block = false,
            Event::Code(_) => codes.push(range),
            // MarkdownIt records local file links as links_to, including reference links.
            Event::Start(Tag::Link { dest_url, .. }) => {
                if let Some(target) = markdown_path(&dest_url) {
                    ordered_relations.push((
                        range.start,
                        Relation {
                            rel_type: "links_to".into(),
                            target,
                            context: None,
                            line_range: lines.range(&range),
                        },
                    ));
                }
            }
            _ => (),
        }
    }
    if let Some(block) = current {
        blocks.push(block);
    }
    for block in blocks {
        let original = &body[block.range.clone()];
        let mut masked = original.as_bytes().to_vec();
        let first_code = codes.partition_point(|code| code.end <= block.range.start);
        for code in codes[first_code..]
            .iter()
            .take_while(|code| code.start < block.range.end)
        {
            let start = code.start.max(block.range.start);
            let end = code.end.min(block.range.end);
            if start < end {
                for b in &mut masked[start - block.range.start..end - block.range.start] {
                    if matches!(*b, b'[' | b']') {
                        *b = b' ';
                    }
                }
            }
        }
        let Ok(masked) = std::str::from_utf8(&masked) else {
            continue;
        };
        let line_range = lines.range(&block.range);
        let content = original.trim();
        let content = if block.heading {
            content
                .trim_start_matches('#')
                .trim()
                .trim_end_matches('#')
                .trim()
        } else {
            content
        };
        if !block.quoted
            && let Some(obs) = observation(content, line_range.clone())
        {
            parsed.observations.push(obs);
        }
        let links = scan_links(masked, original, body_start + block.range.start);
        if block.in_list
            && let Some(relation) = explicit_relation(masked, original, &links, line_range.clone())
        {
            ordered_relations.push((block.range.start, relation));
        } else {
            ordered_relations.extend(links.iter().map(|link| {
                (
                    link.byte_range.start - body_start,
                    Relation {
                        rel_type: "links_to".into(),
                        // Keep the complete authored alias in the relation target; the
                        // dedicated WikiLink view additionally exposes the split alias.
                        target: normalize_reference(
                            body[link.byte_range.start - body_start + 2
                                ..link.byte_range.end - body_start - 2]
                                .trim(),
                        ),
                        context: None,
                        line_range: line_range.clone(),
                    },
                )
            }));
        }
        parsed.links.extend(links);
    }
    ordered_relations.sort_by_key(|(offset, _)| *offset);
    parsed.relations = ordered_relations
        .into_iter()
        .map(|(_, relation)| relation)
        .collect();
    parsed
}

struct LineIndex<'a> {
    bytes: &'a [u8],
    starts: Vec<usize>,
    body_start: usize,
}
impl<'a> LineIndex<'a> {
    fn new(bytes: &'a [u8], body_start: usize) -> Self {
        let mut starts = vec![0];
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' || (*b == b'\r' && bytes.get(i + 1) != Some(&b'\n')) {
                starts.push(i + 1);
            }
        }
        Self {
            bytes,
            starts,
            body_start,
        }
    }
    fn range(&self, range: &Range<usize>) -> Range<usize> {
        let start = self.body_start + range.start;
        let end = self.body_start + range.end;
        let line = |offset| self.starts.partition_point(|p| *p <= offset);
        let end_line =
            line(end) + usize::from(end <= start || !matches!(self.bytes[end - 1], b'\n' | b'\r'));
        line(start)..end_line
    }
}

fn strip_directive(content: &str) -> (&str, bool) {
    let content = content.trim_end();
    if let Some(prefix) = content.strip_suffix("#bm:links_to")
        && prefix.chars().next_back().is_some_and(char::is_whitespace)
    {
        (prefix.trim_end(), true)
    } else {
        (content, false)
    }
}

fn observation(content: &str, line_range: Range<usize>) -> Option<Observation> {
    let (mut content, _) = strip_directive(content);
    if content.starts_with("[ ]") || content.starts_with("[x]") || content.starts_with("[-]") {
        return None;
    }
    if !content.contains('\n')
        && ((content.starts_with("[[") && content.ends_with("]]"))
            || (content.starts_with('[') && content.contains("](") && content.ends_with(')')))
    {
        return None;
    }
    let mut category = None;
    if let Some(capture) = CATEGORY.as_ref().and_then(|re| re.captures(content)) {
        let cat = capture[1].trim();
        let task = cat.chars().count() == 1
            && cat
                .chars()
                .next()
                .is_some_and(|c| matches!(c, 'x' | 'X') || !c.is_alphanumeric());
        if !task && !TIMESTAMP.as_ref().is_some_and(|re| re.is_match(cat)) {
            category = Some(cat.to_string());
            content = capture.get(2)?.as_str().trim();
        }
    }
    let tagged = content.split_whitespace().any(|word| word.starts_with('#'));
    if category.is_none() && !tagged {
        return None;
    }
    if let Some(rest) = content.strip_prefix("[]")
        && rest.chars().next().is_some_and(char::is_whitespace)
    {
        content = rest
            .trim_start()
            .split('\n')
            .next()
            .unwrap_or_default()
            .trim();
    }
    let mut context = None;
    if content.ends_with(')')
        && let Some(start) = content.rfind('(')
    {
        context = Some(content[start + 1..content.len() - 1].trim().to_string());
        content = content[..start].trim();
    }
    if content.is_empty() {
        return None;
    }
    let tags = content
        .split_whitespace()
        .filter_map(|word| word.strip_prefix('#'))
        .flat_map(|word| {
            if word.is_empty() {
                return vec![String::new()];
            }
            word.split('#')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .collect();
    Some(Observation {
        category,
        content: content.to_string(),
        tags,
        context,
        line_range,
    })
}

fn escaped(bytes: &[u8], pos: usize) -> bool {
    bytes[..pos]
        .iter()
        .rev()
        .take_while(|b| **b == b'\\')
        .count()
        % 2
        == 1
}

fn normalize_reference(target: &str) -> String {
    if let Some((project, rest)) = target.split_once("::") {
        format!("{project}/{}", rest.trim_start_matches('/'))
    } else {
        target.to_string()
    }
}

fn scan_links(masked: &str, original: &str, base: usize) -> Vec<WikiLink> {
    let bytes = masked.as_bytes();
    let mut links = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if &bytes[i..i + 2] != b"[[" {
            i += 1;
            continue;
        }
        if escaped(bytes, i) {
            i += 2;
            continue;
        }
        let start = i;
        let mut depth = 1;
        i += 2;
        while i + 1 < bytes.len() {
            if !escaped(bytes, i) {
                if &bytes[i..i + 2] == b"[[" {
                    depth += 1;
                    i += 2;
                    continue;
                }
                if &bytes[i..i + 2] == b"]]" {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }
        if depth != 0 {
            break;
        }
        let text = original[start + 2..i].trim();
        if !text.is_empty() {
            let (target, alias) = text.split_once('|').map_or((text, None), |(t, a)| {
                (t.trim(), Some(a.trim().to_string()))
            });
            links.push(WikiLink {
                target: normalize_reference(target),
                alias,
                byte_range: base + start..base + i + 2,
            });
        }
        i += 2;
    }
    links
}

fn explicit_relation(
    masked: &str,
    original: &str,
    links: &[WikiLink],
    line_range: Range<usize>,
) -> Option<Relation> {
    if strip_directive(masked).1 || links.is_empty() {
        return None;
    }
    let start = masked.find("[[")?;
    let end = masked[start + 2..].find("]]")? + start + 2;
    let label = masked[..start].trim();
    let first = label.chars().next()?;
    let label = if matches!(first, '\'' | '"') && label.ends_with(first) && label.len() >= 2 {
        label[1..label.len() - 1].trim()
    } else {
        if label.chars().any(char::is_whitespace) {
            return None;
        }
        label
    };
    if label.is_empty() {
        return None;
    }
    let tail = masked[end + 2..].trim();
    let mut context = None;
    if !tail.is_empty() {
        if !single_parenthesized(tail) {
            return None;
        }
        let tail = original[end + 2..].trim();
        let value = tail[1..tail.len() - 1].trim();
        if !value.is_empty() {
            context = Some(value.to_string());
        }
    }
    let target = original[start + 2..end].trim();
    if target.is_empty() {
        return None;
    }
    Some(Relation {
        rel_type: label.to_string(),
        target: normalize_reference(target),
        context,
        line_range,
    })
}

fn single_parenthesized(text: &str) -> bool {
    if !text.starts_with('(') {
        return false;
    }
    let mut depth = 0;
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1 == text.len();
                }
            }
            _ => (),
        }
    }
    false
}

fn markdown_path(href: &str) -> Option<String> {
    if href.starts_with("//") {
        return None;
    }
    let path = href.split(['?', '#']).next()?;
    if path.is_empty() || path.split('/').next()?.contains(':') {
        return None;
    }
    let mut bytes = Vec::new();
    let mut i = 0;
    while i < path.len() {
        if path.as_bytes()[i] == b'%'
            && let Some(hex) = path.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            bytes.push(byte);
            i += 3;
            continue;
        }
        bytes.push(path.as_bytes()[i]);
        i += 1;
    }
    let path = String::from_utf8_lossy(&bytes);
    if path.contains(['\\', '\0']) || path.ends_with('/') {
        return None;
    }
    Some(
        if path.starts_with('/') || path.starts_with("./") || path.starts_with("../") {
            path.into()
        } else {
            format!("./{path}")
        },
    )
}
