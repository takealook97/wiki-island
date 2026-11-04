//! The title a delivered note carries. Search, read, list and context all
//! derive it here, so one note never has two titles.

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use serde_json::Value;

/// How much of a body is scanned for its first heading. A heading further
/// down is a section, not the title; the bound also keeps listings cheap.
pub const TITLE_SCAN_BYTES: usize = 4096;

/// The title of the note at `path`: the `summary` of a memory claim, then
/// the frontmatter `title`, then the first H1 near the top of `body`, then
/// the file stem. `metadata` is the parsed frontmatter (`Null` when none).
pub fn hit_title(path: &str, metadata: &Value, body: &str) -> String {
    let field = |key: &str| {
        metadata
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let summary = (field("type").as_deref() == Some("memory"))
        .then(|| field("summary"))
        .flatten();
    summary
        .or_else(|| field("title"))
        .or_else(|| first_heading(body))
        .unwrap_or_else(|| file_stem(path))
}

/// Plain text of the first Markdown H1 in the first [`TITLE_SCAN_BYTES`]
/// of `body` (cut on a line boundary), excluding code examples.
pub fn first_heading(body: &str) -> Option<String> {
    let head = if body.len() > TITLE_SCAN_BYTES {
        let mut end = TITLE_SCAN_BYTES;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        let cut = &body[..end];
        cut.rfind('\n').map_or(cut, |line| &cut[..line + 1])
    } else {
        body
    };
    let mut title: Option<String> = None;
    for event in Parser::new(head) {
        match event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H1,
                ..
            }) => title = Some(String::new()),
            Event::Text(text) | Event::Code(text) => {
                if let Some(title) = &mut title {
                    title.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(title) = &mut title {
                    title.push(' ');
                }
            }
            Event::End(TagEnd::Heading(HeadingLevel::H1)) => {
                if let Some(title) = title.take().filter(|s| !s.trim().is_empty()) {
                    return Some(title);
                }
            }
            _ => {}
        }
    }
    None
}

fn file_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn title_order_is_summary_title_heading_stem() {
        let memory = json!({"type":"memory","title":"T","summary":"S"});
        assert_eq!(hit_title("a/claim.md", &memory, "# H\n"), "S");
        let plain = json!({"title":"T","summary":"S"});
        assert_eq!(hit_title("a/n.md", &plain, "# H\n"), "T");
        assert_eq!(
            hit_title("a/n.md", &Value::Null, "intro\n\n# H `x`\n"),
            "H x"
        );
        assert_eq!(hit_title("a/n.md", &json!({"title":"  "}), "## Not\n"), "n");
    }

    #[test]
    fn a_heading_far_down_is_not_the_title() {
        let body = format!("{}\n# Late\n", "word ".repeat(TITLE_SCAN_BYTES));
        assert_eq!(first_heading(&body), None);
        let body = format!("# 제목\n{}", "가".repeat(TITLE_SCAN_BYTES));
        assert_eq!(first_heading(&body).as_deref(), Some("제목"));
    }
}
