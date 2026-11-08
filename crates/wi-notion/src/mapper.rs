use crate::{Error, Result};
use pulldown_cmark::{CodeBlockKind, Event, LinkType, Options, Parser, Tag};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// JSON blocks plus explicit diagnostics for syntax rendered as plain text.
#[derive(Debug, Clone)]
pub struct MappedNote {
    pub blocks: Vec<Value>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
struct Node<'a> {
    event: Event<'a>,
    children: Vec<Node<'a>>,
}

fn nodes<'a>(events: &mut impl Iterator<Item = Event<'a>>) -> Vec<Node<'a>> {
    let mut result = Vec::new();
    while let Some(event) = events.next() {
        if matches!(event, Event::End(_)) {
            break;
        }
        let children = if matches!(event, Event::Start(_)) {
            nodes(events)
        } else {
            Vec::new()
        };
        result.push(Node { event, children });
    }
    result
}

/// Maps a note without changing its canonical bytes. Known wikilinks are page
/// mentions; unknown targets and local links remain visible text.
pub fn map_markdown(bytes: &[u8], known_pages: &BTreeMap<String, String>) -> Result<MappedNote> {
    let note = wi_note::Note::parse(bytes).map_err(|_| Error::Utf8)?;
    let mut warnings: Vec<String> = note
        .diagnostics()
        .iter()
        .map(|d| d.message.clone())
        .collect();
    let mut blocks = vec![block(
        "callout",
        text("Read-only view; edit in your notes folder"),
    )];
    blocks[0]["callout"]["icon"] = json!({"type":"emoji", "emoji":"📝"});
    if let Some(fm) = note.frontmatter() {
        let properties = fm
            .entries
            .iter()
            .map(|e| format!("{}: {}", e.key, e.value))
            .collect::<Vec<_>>()
            .join(" · ");
        if !properties.is_empty() {
            blocks.extend(text_blocks("paragraph", text(&properties)));
        }
    }
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_WIKILINKS;
    let tree = nodes(&mut Parser::new_ext(note.body(), options));
    blocks.extend(render(&tree, known_pages, &mut warnings)?);
    Ok(MappedNote { blocks, warnings })
}

pub(crate) fn text(value: &str) -> Vec<Value> {
    // UTF-16 counting also keeps supplementary characters within the API limit.
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut units = 0;
    for ch in value.chars() {
        if units + ch.len_utf16() > 2000 {
            chunks.push(json!({"type":"text", "text":{"content":current}}));
            current = String::new();
            units = 0;
        }
        current.push(ch);
        units += ch.len_utf16();
    }
    if !current.is_empty() {
        chunks.push(json!({"type":"text", "text":{"content":current}}));
    }
    chunks
}

fn block(kind: &str, rich_text: Vec<Value>) -> Value {
    json!({"object":"block", "type":kind, kind:{"rich_text":rich_text}})
}

fn text_blocks(kind: &str, rich_text: Vec<Value>) -> Vec<Value> {
    if rich_text.is_empty() {
        return vec![block(kind, Vec::new())];
    }
    rich_text
        .chunks(100)
        .map(|chunk| block(kind, chunk.to_vec()))
        .collect()
}

fn inline(
    nodes: &[Node<'_>],
    known: &BTreeMap<String, String>,
    annotations: &Value,
    link: Option<&str>,
    warnings: &mut Vec<String>,
) -> Vec<Value> {
    let mut output = Vec::new();
    for node in nodes {
        let mut style = annotations.clone();
        let mut url = link;
        let value = match &node.event {
            Event::Text(value) => Some(value.to_string()),
            Event::Code(value) => {
                style["code"] = json!(true);
                Some(value.to_string())
            }
            Event::SoftBreak | Event::HardBreak => Some("\n".to_owned()),
            Event::Html(value) | Event::InlineHtml(value) => {
                warnings.push("HTML is rendered as plain text.".to_owned());
                Some(value.to_string())
            }
            Event::Start(Tag::Strong) => {
                style["bold"] = json!(true);
                None
            }
            Event::Start(Tag::Emphasis) => {
                style["italic"] = json!(true);
                None
            }
            Event::Start(Tag::Strikethrough) => {
                style["strikethrough"] = json!(true);
                None
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) => {
                if matches!(link_type, LinkType::WikiLink { .. }) {
                    if let Some(page) = known.get(dest_url.as_ref()) {
                        output.push(json!({"type":"mention", "mention":{"type":"page", "page":{"id":page}}, "annotations":style}));
                        continue;
                    }
                } else if (dest_url.starts_with("https://")
                    || dest_url.starts_with("http://")
                    || dest_url.starts_with("mailto:"))
                    && dest_url.encode_utf16().count() <= 2000
                {
                    url = Some(dest_url);
                } else {
                    warnings
                        .push("A local or unsupported link is rendered as plain text.".to_owned());
                }
                None
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                warnings.push(
                    "Images and attachments are rendered as text links; files are not uploaded."
                        .to_owned(),
                );
                output.extend(inline(&node.children, known, &style, None, warnings));
                Some(format!(" ({dest_url})"))
            }
            _ => None,
        };
        if let Some(value) = value {
            for mut item in text(&value) {
                item["annotations"] = style.clone();
                if let Some(url) = url {
                    item["text"]["link"] = json!({"url":url});
                }
                output.push(item);
            }
        } else {
            output.extend(inline(&node.children, known, &style, url, warnings));
        }
    }
    output
}

fn rich(
    nodes: &[Node<'_>],
    known: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) -> Vec<Value> {
    inline(nodes, known, &json!({}), None, warnings)
}

fn task(nodes: &[Node<'_>]) -> Option<bool> {
    nodes.iter().find_map(|node| match node.event {
        Event::TaskListMarker(checked) => Some(checked),
        Event::Start(Tag::Paragraph) => task(&node.children),
        _ => None,
    })
}

fn render(
    nodes: &[Node<'_>],
    known: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    let mut loose = Vec::new();
    for node in nodes {
        if !matches!(node.event, Event::Start(_) | Event::Rule) {
            loose.extend(rich(std::slice::from_ref(node), known, warnings));
            continue;
        }
        if !loose.is_empty() {
            output.extend(text_blocks("paragraph", std::mem::take(&mut loose)));
        }
        match &node.event {
            Event::Rule => output.push(json!({"object":"block", "type":"divider", "divider":{}})),
            Event::Start(Tag::Paragraph) => output.extend(text_blocks(
                "paragraph",
                rich(&node.children, known, warnings),
            )),
            Event::Start(Tag::Heading { level, .. }) => {
                let kind = format!("heading_{}", (*level as u8).min(3));
                output.extend(text_blocks(&kind, rich(&node.children, known, warnings)));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("plain text")
                    }
                    _ => "plain text",
                };
                // Unknown language names use plain text instead of an invalid API enum.
                let language = match language {
                    "rust" | "python" | "javascript" | "typescript" | "json" | "bash" | "sql"
                    | "html" | "css" | "java" | "go" | "c" | "c++" | "yaml" | "markdown" => {
                        language
                    }
                    _ => "plain text",
                };
                for mut value in
                    text_blocks("code", rich(&node.children, &BTreeMap::new(), warnings))
                {
                    value["code"]["language"] = json!(language);
                    output.push(value);
                }
            }
            Event::Start(Tag::List(start)) => {
                for item in &node.children {
                    let checked = task(&item.children);
                    let kind = if checked.is_some() {
                        "to_do"
                    } else if start.is_some() {
                        "numbered_list_item"
                    } else {
                        "bulleted_list_item"
                    };
                    output.push(container(kind, &item.children, checked, known, warnings)?);
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                output.push(container("quote", &node.children, None, known, warnings)?)
            }
            Event::Start(Tag::Table(columns)) => {
                if columns.len() > 100 {
                    return Err(Error::Limit("tables support at most 100 columns"));
                }
                let mut rows = Vec::new();
                for row in &node.children {
                    let mut cells = Vec::new();
                    for cell in &row.children {
                        let value = rich(&cell.children, known, warnings);
                        if value.len() > 100 {
                            return Err(Error::Limit("table cell has over 100 rich-text elements"));
                        }
                        cells.push(value);
                    }
                    rows.push(
                        json!({"object":"block", "type":"table_row", "table_row":{"cells":cells}}),
                    );
                }
                output.push(json!({"object":"block", "type":"table", "table":{"table_width":columns.len(), "has_column_header":true, "has_row_header":false, "children":rows}}));
            }
            Event::Start(Tag::HtmlBlock) => output.extend(text_blocks(
                "paragraph",
                rich(&node.children, known, warnings),
            )),
            _ => output.extend(render(&node.children, known, warnings)?),
        }
    }
    if !loose.is_empty() {
        output.extend(text_blocks("paragraph", loose));
    }
    Ok(output)
}

fn container(
    kind: &str,
    nodes: &[Node<'_>],
    checked: Option<bool>,
    known: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) -> Result<Value> {
    let mut children = render(nodes, known, warnings)?;
    let rich_text = if children.first().is_some_and(|b| b["type"] == "paragraph") {
        children.remove(0)["paragraph"]["rich_text"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut value = block(kind, rich_text);
    if let Some(checked) = checked {
        value[kind]["checked"] = json!(checked);
    }
    if !children.is_empty() {
        value[kind]["children"] = json!(children);
    }
    Ok(value)
}
