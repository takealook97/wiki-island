use crate::engine::Error;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use std::ops::Range;

/// Heading selectors use paths and zero-based occurrence syntax.
/// Markdown parsing excludes fenced code and includes Setext headings.
pub fn select(text: &str, selector: &str) -> Result<Range<usize>, Error> {
    select_with(text, selector, true)
}
pub fn select_with(text: &str, selector: &str, subsections: bool) -> Result<Range<usize>, Error> {
    let note = wi_note::Note::parse(text.as_bytes()).map_err(wi_store::StoreError::from)?;
    let base = note.body_start();
    let mut headings: Vec<(usize, usize, String)> = Vec::new();
    let mut pending: Option<(usize, usize, String)> = None;
    for (event, range) in Parser::new(note.body()).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                pending = Some((range.start + base, level as usize, String::new()))
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, _, title)) = &mut pending {
                    title.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, _, title)) = &mut pending {
                    title.push(' ');
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(h) = pending.take() {
                    headings.push(h);
                }
            }
            _ => {}
        }
    }
    let (selector, occurrence) = if selector.ends_with(']') {
        let Some((name, index)) = selector.rsplit_once('[') else {
            return Err(Error::Invalid("Invalid heading occurrence selector."));
        };
        let index = index
            .trim_end_matches(']')
            .parse::<usize>()
            .map_err(|_| Error::Invalid("Invalid heading occurrence selector."))?;
        (name, Some(index))
    } else {
        (selector, None)
    };
    let wanted = selector.split('/').collect::<Vec<_>>();
    if wanted.iter().any(|s| s.is_empty()) {
        return Err(Error::Invalid("Invalid heading path."));
    }
    let mut parents: Vec<(usize, &str)> = Vec::new();
    let mut matches = Vec::new();
    for (i, (_, level, title)) in headings.iter().enumerate() {
        while parents.last().is_some_and(|(l, _)| l >= level) {
            parents.pop();
        }
        parents.push((*level, title));
        if title == wanted[wanted.len() - 1]
            && (wanted.len() == 1
                || (parents.len() >= wanted.len()
                    && parents[parents.len() - wanted.len()..]
                        .iter()
                        .map(|(_, t)| *t)
                        .eq(wanted.iter().copied())))
        {
            matches.push(i);
        }
    }
    let index = if let Some(occurrence) = occurrence {
        matches.get(occurrence).copied().ok_or(Error::NotFound)?
    } else if matches.is_empty() {
        return Err(Error::NotFound);
    } else if matches.len() == 1 {
        matches[0]
    } else {
        return Err(Error::Invalid(
            "Section is missing or ambiguous. Use a heading path or zero-based occurrence, such as Decisions[1].",
        ));
    };
    let start = headings[index].0;
    let level = headings[index].1;
    let end = headings[index + 1..]
        .iter()
        .find(|(_, l, _)| !subsections || *l <= level)
        .map_or(text.len(), |(start, _, _)| *start);
    Ok(start..end)
}
