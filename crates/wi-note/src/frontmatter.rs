/// key written by imports before 2026-11-07
pub const LEGACY_PARSE_SEMANTICS_KEY: &str = "bm_parse_semantics";

use serde::Serialize;
use std::collections::HashSet;
use std::ops::Range;
use thiserror::Error;
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};
use yaml_rust2::{Yaml, YamlLoader};

pub use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Error)]
pub enum ReadOnlyReason {
    #[error("unterminated frontmatter")]
    Unterminated,
    #[error("invalid YAML: {0}")]
    InvalidYaml(String),
    #[error("duplicate top-level key: {0}")]
    DuplicateKey(String),
    #[error("frontmatter must be a mapping")]
    NonMapping,
    #[error("flow-style top-level mapping")]
    FlowMapping,
    #[error("anchors and aliases are read-only")]
    AnchorsOrAliases,
    #[error("multiple YAML documents")]
    MultipleDocuments,
    #[error("tabs in indentation")]
    TabIndentation,
    #[error("unsupported key or value syntax: {0}")]
    UnsupportedSyntax(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub field: Option<String>,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("note is not UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
}

#[derive(Debug, Error)]
pub enum EditError {
    #[error("frontmatter is read-only: {0}")]
    ReadOnly(#[from] ReadOnlyReason),
    #[error("invalid key: {0}")]
    InvalidKey(String),
    #[error("edit did not round-trip to the requested value")]
    Encoding,
    #[error("removal would change another key: {0}")]
    WouldChangeOtherKey(String),
    #[error(transparent)]
    Parse(#[from] ParseError),
}

/// A key's span covers its key line and value, excluding adjacent standalone comments.
/// All offsets are UTF-8 byte offsets into the original note.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub key: String,
    pub value: Value,
    pub byte_range: Range<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Frontmatter {
    pub entries: Vec<Entry>,
    pub byte_range: Range<usize>,
    pub content_range: Range<usize>,
    reasons: Vec<ReadOnlyReason>,
    indent: String,
}

impl Frontmatter {
    pub fn get(&self, key: &str) -> Option<Value> {
        self.entries
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.value.clone())
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.key.as_str())
    }

    pub fn is_editable(&self) -> Result<(), ReadOnlyReason> {
        self.reasons.first().cloned().map_or(Ok(()), Err)
    }

    pub fn read_only_reasons(&self) -> &[ReadOnlyReason] {
        &self.reasons
    }
}

/// An immutable note. Parsing never normalizes or rewrites original bytes.
/// Malformed metadata is retained with diagnostics; only invalid UTF-8 is a hard error.
#[derive(Debug, Clone)]
pub struct Note {
    raw: Vec<u8>,
    pub(crate) parsed: std::sync::OnceLock<crate::markdown::Parsed>,
    lifecycle: std::sync::OnceLock<crate::Lifecycle>,
    pub(crate) frontmatter: Option<Frontmatter>,
    pub(crate) body_start: usize,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

impl Note {
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        let text = std::str::from_utf8(bytes)?;
        let bom = if text.starts_with('\u{feff}') { 3 } else { 0 };
        let mut note = Self {
            raw: bytes.to_vec(),
            parsed: std::sync::OnceLock::new(),
            lifecycle: std::sync::OnceLock::new(),
            frontmatter: None,
            body_start: bom,
            diagnostics: Vec::new(),
        };
        let lines = lines_at(&text[bom..], bom);
        let opening = lines
            .iter()
            .position(|(_, s)| !line_text(s).trim().is_empty());
        let Some(opening) = opening.filter(|i| is_fence(lines[*i].1)) else {
            return Ok(note);
        };
        let content_start = lines[opening].0 + lines[opening].1.len();
        let close = lines.iter().skip(opening + 1).find(|(_, s)| is_fence(s));
        let (content_end, end) = match close {
            Some((offset, line)) => (*offset, *offset + line.len()),
            None => (text.len(), text.len()),
        };
        let mut fm = parse_frontmatter(&text[content_start..content_end], content_start);
        fm.byte_range = lines[opening].0..end;
        if close.is_none() {
            fm.reasons.insert(0, ReadOnlyReason::Unterminated);
        }
        // Invalid fenced content remains plain Markdown for safe, lossless fallback.
        let malformed = fm.reasons.iter().any(|r| {
            matches!(
                r,
                ReadOnlyReason::Unterminated
                    | ReadOnlyReason::InvalidYaml(_)
                    | ReadOnlyReason::DuplicateKey(_)
                    | ReadOnlyReason::NonMapping
                    | ReadOnlyReason::MultipleDocuments
            )
        });
        if !malformed {
            note.body_start = end;
        }
        note.diagnostics
            .extend(fm.reasons.iter().map(|r| Diagnostic {
                field: None,
                message: r.to_string(),
            }));
        note.frontmatter = Some(fm);
        let (lifecycle, diagnostics) = crate::Lifecycle::from_note(&note);
        let _ = note.lifecycle.set(lifecycle);
        note.diagnostics.extend(diagnostics);
        Ok(note)
    }

    pub fn frontmatter(&self) -> Option<&Frontmatter> {
        self.frontmatter.as_ref()
    }
    pub fn body_start(&self) -> usize {
        self.body_start
    }
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn to_bytes(&self) -> &[u8] {
        &self.raw
    }

    pub fn body(&self) -> &str {
        // The only constructor validates UTF-8 and all edit operations reparse.
        std::str::from_utf8(&self.raw[self.body_start..]).unwrap_or_default()
    }

    pub fn lifecycle(&self) -> crate::Lifecycle {
        self.lifecycle
            .get_or_init(|| crate::Lifecycle::from_note(self).0)
            .clone()
    }

    pub(crate) fn semantics_enabled(&self) -> bool {
        !self
            .frontmatter
            .as_ref()
            .and_then(|f| {
                f.get("parse_semantics")
                    .or_else(|| f.get(LEGACY_PARSE_SEMANTICS_KEY))
            })
            .is_some_and(|v| {
                v == Value::Bool(false)
                    || v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("false"))
            })
    }
}

fn line_text(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}
fn is_fence(line: &str) -> bool {
    line_text(line).trim_end_matches([' ', '\t']) == "---"
}

fn lines_at(text: &str, base: usize) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut iter = text.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if matches!(c, '\n' | '\r') {
            let mut end = i + c.len_utf8();
            if c == '\r' && iter.peek().is_some_and(|(_, c)| *c == '\n') {
                iter.next();
                end += 1;
            }
            lines.push((base + start, &text[start..end]));
            start = end;
        }
    }
    if start < text.len() {
        lines.push((base + start, &text[start..]));
    }
    lines
}

#[derive(Default)]
struct Events(Vec<(Event, Marker)>);
impl MarkedEventReceiver for Events {
    fn on_event(&mut self, ev: Event, mark: Marker) {
        self.0.push((ev, mark));
    }
}

fn json_value(yaml: &Yaml) -> Result<Value, String> {
    match yaml {
        Yaml::Null => Ok(Value::Null),
        Yaml::Boolean(v) => Ok(Value::Bool(*v)),
        Yaml::Integer(v) => Ok((*v).into()),
        Yaml::Real(v) => v
            .parse::<serde_json::Number>()
            .map(Value::Number)
            .map_err(|_| "non-finite or unsupported YAML number".into()),
        Yaml::String(v) => Ok(Value::String(v.clone())),
        Yaml::Array(v) => v
            .iter()
            .map(json_value)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Yaml::Hash(v) => v
            .iter()
            .map(|(k, v)| {
                let key = k
                    .as_str()
                    .ok_or_else(|| "non-string mapping key".to_string())?;
                Ok((key.to_string(), json_value(v)?))
            })
            .collect::<Result<serde_json::Map<_, _>, String>>()
            .map(Value::Object),
        _ => Err("unresolved YAML value".into()),
    }
}

fn parse_frontmatter(source: &str, base: usize) -> Frontmatter {
    let mut fm = Frontmatter {
        entries: Vec::new(),
        byte_range: base..base + source.len(),
        content_range: base..base + source.len(),
        reasons: Vec::new(),
        indent: String::new(),
    };
    if source.trim().starts_with('{') {
        fm.reasons.push(ReadOnlyReason::FlowMapping);
    }
    if source.lines().any(|l| {
        l.chars()
            .take_while(|c| c.is_whitespace())
            .any(|c| c == '\t')
    }) {
        fm.reasons.push(ReadOnlyReason::TabIndentation);
    }
    if source.lines().any(|line| line.trim_end() == "...") {
        fm.reasons.push(ReadOnlyReason::UnsupportedSyntax(
            "explicit YAML document terminator".into(),
        ));
    }
    let mut events = Events::default();
    if let Err(e) = Parser::new_from_str(source).load(&mut events, true) {
        fm.reasons.push(ReadOnlyReason::InvalidYaml(e.to_string()));
        return fm;
    }
    if events
        .0
        .iter()
        .filter(|(e, _)| matches!(e, Event::DocumentStart))
        .count()
        > 1
    {
        fm.reasons.push(ReadOnlyReason::MultipleDocuments);
    }
    if events.0.iter().any(|(e, _)| {
        matches!(
            e,
            Event::Alias(_)
                | Event::Scalar(_, _, 1.., _)
                | Event::MappingStart(1.., _)
                | Event::SequenceStart(1.., _)
        )
    }) {
        fm.reasons.push(ReadOnlyReason::AnchorsOrAliases);
    }
    let docs = match YamlLoader::load_from_str(source) {
        Ok(docs) => docs,
        Err(e) => {
            fm.reasons.push(ReadOnlyReason::InvalidYaml(e.to_string()));
            return fm;
        }
    };
    let empty = source
        .lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'));
    let mapping = docs.first().and_then(Yaml::as_hash);
    if mapping.is_none() && !empty {
        fm.reasons.push(ReadOnlyReason::NonMapping);
        return fm;
    }
    let Some(mapping) = mapping else {
        return fm;
    };
    let lines = lines_at(source, base);
    let mut depth = 0usize;
    let mut expecting_key = true;
    let mut keys = Vec::new();
    let mut seen = HashSet::new();
    for (event, mark) in &events.0 {
        let node = matches!(
            event,
            Event::Scalar(..)
                | Event::Alias(_)
                | Event::MappingStart(..)
                | Event::SequenceStart(..)
        );
        if depth == 1 && node {
            if expecting_key {
                if let Event::Scalar(key, _, _, _) = event {
                    if !seen.insert(key.clone()) {
                        fm.reasons.push(ReadOnlyReason::DuplicateKey(key.clone()));
                    }
                    let line = mark.line().saturating_sub(1);
                    if let Some((offset, text)) = lines.get(line) {
                        // Explicit, multi-line and non-string keys cannot be patched safely.
                        let prefix: String = text.chars().take(mark.col()).collect();
                        if !prefix.chars().all(|c| c == ' ') || key.contains(['\n', '\r']) {
                            fm.reasons
                                .push(ReadOnlyReason::UnsupportedSyntax("complex key".into()));
                        }
                        if keys.is_empty() {
                            fm.indent = prefix;
                        }
                        keys.push((key.clone(), *offset, line));
                    }
                } else {
                    fm.reasons
                        .push(ReadOnlyReason::UnsupportedSyntax("complex key".into()));
                }
            }
            expecting_key = !expecting_key;
        }
        match event {
            Event::MappingStart(..) | Event::SequenceStart(..) => depth += 1,
            Event::MappingEnd | Event::SequenceEnd => depth = depth.saturating_sub(1),
            _ => (),
        }
    }
    // Scalar events distinguish an indented literal '# text' from a YAML comment.
    let mut scalar_lines = HashSet::new();
    for (event, mark) in &events.0 {
        if matches!(
            event,
            Event::Scalar(_, TScalarStyle::Literal | TScalarStyle::Folded, _, _)
        ) {
            // yaml-rust2 marks block scalars at their first content character,
            // rather than at the '|' or '>' indicator.
            let first = mark.line().saturating_sub(1);
            let indent = mark.col();
            if indent > fm.indent.len() {
                for (i, (_, text)) in lines.iter().enumerate().skip(first) {
                    if !text.trim().is_empty()
                        && text.chars().take_while(|c| *c == ' ').count() < indent
                    {
                        break;
                    }
                    scalar_lines.insert(i);
                }
            }
        }
    }
    // Key markers supply structure; trailing standalone comments/blank lines are excluded.
    for (idx, (key, start, line)) in keys.iter().enumerate() {
        let stop_line = keys.get(idx + 1).map_or(lines.len(), |k| k.2);
        let mut end_line = stop_line;
        while end_line > *line + 1 {
            let text = line_text(lines[end_line - 1].1);
            let standalone = !scalar_lines.contains(&(end_line - 1))
                && (text.trim().is_empty() || text.trim_start().starts_with('#'));
            if !standalone {
                break;
            }
            end_line -= 1;
        }
        let end = lines.get(end_line).map_or(base + source.len(), |l| l.0);
        let ykey = Yaml::String(key.clone());
        let Some(value) = mapping.get(&ykey) else {
            fm.reasons.push(ReadOnlyReason::UnsupportedSyntax(
                "non-string mapping key".into(),
            ));
            continue;
        };
        match json_value(value) {
            Ok(value) => fm.entries.push(Entry {
                key: key.clone(),
                value,
                byte_range: *start..end,
            }),
            Err(reason) => fm.reasons.push(ReadOnlyReason::UnsupportedSyntax(reason)),
        }
    }
    fm
}

/// Encode only the requested value, testing plain scalars in the actual
/// block/flow context. Collections use flow syntax and never emit YAML aliases.
fn encode(value: &Value) -> String {
    encode_at(value, false)
}

fn encode_at(value: &Value, flow: bool) -> String {
    match value {
        Value::String(s) => {
            let candidate = if flow {
                format!("[{s}]")
            } else {
                format!("k: {s}\n")
            };
            let expected = Yaml::String(s.clone());
            let plain = plain_candidate(s)
                && YamlLoader::load_from_str(&candidate)
                    .ok()
                    .is_some_and(|docs| {
                        docs.len() == 1
                            && if flow {
                                docs[0]
                                    .as_vec()
                                    .is_some_and(|v| v.len() == 1 && v[0] == expected)
                            } else {
                                docs[0]["k"] == expected
                            }
                    });
            if plain { s.clone() } else { quoted(s) }
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|v| encode_at(v, true))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(k, v)| format!("{}: {}", encode_key(k, true), encode_at(v, true)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => value.to_string(),
    }
}

fn plain_candidate(text: &str) -> bool {
    // Intersection of conservative YAML 1.1 and YAML 1.2 string syntax.
    text.chars().next().is_some_and(char::is_alphabetic)
        && text.trim() == text
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '_' | '/' | '-'))
        && !matches!(
            text.to_ascii_lowercase().as_str(),
            "y" | "yes" | "n" | "no" | "on" | "off" | "true" | "false" | "null"
        )
}

fn quoted(text: &str) -> String {
    // YAML treats these Unicode code points as line breaks even inside quotes;
    // JSON leaves them literal by default.
    Value::String(text.into())
        .to_string()
        .replace('\u{85}', "\\u0085")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn encode_key(key: &str, flow: bool) -> String {
    let candidate = if flow {
        format!("{{{key}: null}}")
    } else {
        format!("{key}: null\n")
    };
    let plain = plain_candidate(key)
        && YamlLoader::load_from_str(&candidate)
            .ok()
            .is_some_and(|docs| {
                docs.len() == 1
                    && docs[0].as_hash().is_some_and(|m| {
                        m.len() == 1 && m.get(&Yaml::String(key.into())) == Some(&Yaml::Null)
                    })
            });
    if plain { key.into() } else { quoted(key) }
}

pub fn set_key(note: &Note, key: &str, value: &Value) -> Result<Note, EditError> {
    if key.is_empty() || key.contains(['\n', '\r', '\0']) {
        return Err(EditError::InvalidKey(key.into()));
    }
    let bom = if note.to_bytes().starts_with(&[0xef, 0xbb, 0xbf]) {
        3
    } else {
        0
    };
    let text = std::str::from_utf8(note.to_bytes()).map_err(ParseError::from)?;
    let start = note
        .frontmatter
        .as_ref()
        .map_or(bom, |fm| fm.byte_range.start);
    let first = lines_at(&text[start..], start)
        .first()
        .map(|(_, line)| *line)
        .unwrap_or("");
    let newline = if first.ends_with("\r\n") {
        "\r\n"
    } else if first.ends_with('\r') {
        "\r"
    } else {
        "\n"
    };
    let encoded_key = encode_key(key, false);
    let (range, replacement) = if let Some(fm) = &note.frontmatter {
        fm.is_editable()?;
        let range = fm
            .entries
            .iter()
            .find(|e| e.key == key)
            .map_or(fm.content_range.end..fm.content_range.end, |e| {
                e.byte_range.clone()
            });
        (
            range,
            format!("{}{encoded_key}: {}{newline}", fm.indent, encode(value)),
        )
    } else {
        (
            bom..bom,
            format!(
                "---{newline}{encoded_key}: {}{newline}---{newline}",
                encode(value)
            ),
        )
    };
    let edited = replace(note, range, replacement.as_bytes())?;
    let fm = edited.frontmatter.as_ref().ok_or(EditError::Encoding)?;
    fm.is_editable().map_err(|_| EditError::Encoding)?;
    if fm.get(key).as_ref() != Some(value) {
        return Err(EditError::Encoding);
    }
    Ok(edited)
}

pub fn remove_key(note: &Note, key: &str) -> Result<Note, EditError> {
    let Some(fm) = &note.frontmatter else {
        return Ok(note.clone());
    };
    fm.is_editable()?;
    match fm.entries.iter().find(|e| e.key == key) {
        Some(entry) => {
            let edited = replace(note, entry.byte_range.clone(), b"")?;
            let next = edited.frontmatter.as_ref().ok_or(EditError::Encoding)?;
            next.is_editable().map_err(|_| EditError::Encoding)?;
            for other in fm.entries.iter().filter(|e| e.key != key) {
                if next.get(&other.key).as_ref() != Some(&other.value) {
                    return Err(EditError::WouldChangeOtherKey(other.key.clone()));
                }
            }
            Ok(edited)
        }
        None => Ok(note.clone()),
    }
}

fn replace(note: &Note, range: Range<usize>, value: &[u8]) -> Result<Note, EditError> {
    let mut bytes = Vec::with_capacity(note.raw.len() + value.len());
    bytes.extend_from_slice(&note.raw[..range.start]);
    bytes.extend_from_slice(value);
    bytes.extend_from_slice(&note.raw[range.end..]);
    Ok(Note::parse(&bytes)?)
}
