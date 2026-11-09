//! Office Open XML (docx, pptx) text extraction.
use super::{MAX_EXPANDED, MAX_PAGES, MAX_XML};
use crate::{Error, Result};
use quick_xml::{Reader, events::Event};
use std::io::{Cursor, Read};
use std::time::Instant;
use zip::ZipArchive;

fn xml_text(bytes: &[u8]) -> Result<String> {
    let mut reader = Reader::from_reader(bytes);
    let mut out = String::new();
    let mut inside_text = false;
    let mut cell_start = None;
    let mut row_cells = 0;
    loop {
        match reader
            .read_event()
            .map_err(|e| Error::Extraction(e.to_string()))?
        {
            Event::Start(e) => {
                if e.local_name().as_ref() == b"tr" {
                    row_cells = 0;
                }
                if e.local_name().as_ref() == b"tc" {
                    if row_cells > 0 {
                        out.push_str(" | ");
                    }
                    cell_start = Some(out.len());
                    row_cells += 1;
                }
                if e.local_name().as_ref() == b"t" {
                    inside_text = true;
                }
            }
            Event::Text(e) if inside_text => {
                let decoded = e.decode().map_err(|e| Error::Extraction(e.to_string()))?;
                out.push_str(
                    &quick_xml::escape::unescape(&decoded)
                        .map_err(|e| Error::Extraction(e.to_string()))?,
                );
            }
            Event::GeneralRef(e) if inside_text => {
                let name = e.decode().map_err(|e| Error::Extraction(e.to_string()))?;
                let escaped = format!("&{name};");
                out.push_str(
                    &quick_xml::escape::unescape(&escaped)
                        .map_err(|e| Error::Extraction(e.to_string()))?,
                );
            }
            Event::CData(e) if inside_text => {
                out.push_str(&e.decode().map_err(|e| Error::Extraction(e.to_string()))?)
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"t" => inside_text = false,
                b"p" => out.push(if cell_start.is_some() { ' ' } else { '\n' }),
                b"tc" => {
                    if let Some(start) = cell_start.take() {
                        let cell = out[start..]
                            .trim()
                            .replace('|', "\\|")
                            .replace('\n', "\\n")
                            .replace('\t', "\\t");
                        out.truncate(start);
                        out.push_str(&cell);
                    }
                }
                b"tr" => out.push('\n'),
                _ => (),
            },
            Event::Empty(e) => match e.local_name().as_ref() {
                b"tab" => out.push('\t'),
                b"br" | b"cr" => out.push('\n'),
                _ => (),
            },
            Event::DocType(_) => {
                return Err(Error::Extraction(
                    "XML document types are unsupported".into(),
                ));
            }
            Event::Eof => break,
            _ => (),
        }
    }
    Ok(out)
}

fn read_zip(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    name: &str,
    expanded: &mut usize,
) -> Result<Vec<u8>> {
    let file = archive
        .by_name(name)
        .map_err(|e| Error::Extraction(e.to_string()))?;
    if file.size() > MAX_EXPANDED.saturating_sub(*expanded) as u64 {
        return Err(Error::Extraction(
            "document expansion exceeds 256 MiB".into(),
        ));
    }
    if file.size() > MAX_XML {
        return Err(Error::Extraction("XML member exceeds 64 MiB limit".into()));
    }
    let mut bytes = Vec::new();
    let remaining = MAX_EXPANDED.saturating_sub(*expanded);
    file.take(MAX_XML.min(remaining as u64) + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_XML {
        return Err(Error::Extraction("XML expansion limit".into()));
    }
    *expanded += bytes.len();
    if *expanded > MAX_EXPANDED {
        return Err(Error::Extraction(
            "document expansion exceeds 256 MiB".into(),
        ));
    }
    Ok(bytes)
}

pub(super) fn pptx(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    expanded: &mut usize,
    deadline: Instant,
    skipped: &mut Vec<usize>,
) -> Result<Vec<String>> {
    // Presentation relationship order is authoritative (filenames need not be).
    let relationships = read_zip(archive, "ppt/_rels/presentation.xml.rels", expanded)?;
    let mut rels = std::collections::BTreeMap::new();
    let mut reader = Reader::from_reader(relationships.as_slice());
    loop {
        match reader
            .read_event()
            .map_err(|e| Error::Extraction(e.to_string()))?
        {
            Event::Empty(e) | Event::Start(e) if e.local_name().as_ref() == b"Relationship" => {
                let mut id = None;
                let mut target = None;
                let mut external = false;
                for a in e.attributes() {
                    let a = a.map_err(|e| Error::Extraction(e.to_string()))?;
                    let value = a
                        .unescape_value()
                        .map_err(|e| Error::Extraction(e.to_string()))?
                        .into_owned();
                    match a.key.as_ref() {
                        b"Id" => id = Some(value),
                        b"Target" => target = Some(value),
                        b"TargetMode" => external = value == "External",
                        _ => (),
                    }
                }
                if !external && let (Some(id), Some(target)) = (id, target) {
                    rels.insert(id, target);
                }
            }
            Event::Eof => break,
            _ => (),
        }
    }
    let presentation = read_zip(archive, "ppt/presentation.xml", expanded)?;
    let mut reader = Reader::from_reader(presentation.as_slice());
    let mut pages = Vec::new();
    let mut total = 0;
    let mut cache = std::collections::BTreeMap::<String, String>::new();
    loop {
        match reader
            .read_event()
            .map_err(|e| Error::Extraction(e.to_string()))?
        {
            Event::Empty(e) | Event::Start(e) if e.local_name().as_ref() == b"sldId" => {
                let mut id = None;
                for a in e.attributes() {
                    let a = a.map_err(|e| Error::Extraction(e.to_string()))?;
                    if a.key.as_ref() == b"r:id" {
                        id = Some(
                            a.unescape_value()
                                .map_err(|e| Error::Extraction(e.to_string()))?
                                .into_owned(),
                        );
                    }
                }
                if pages.len() >= MAX_PAGES {
                    return Err(Error::Extraction("page limit".into()));
                }
                if Instant::now() >= deadline {
                    pages.push(String::new());
                    skipped.push(pages.len());
                    continue;
                }
                let relationship = id
                    .as_ref()
                    .ok_or_else(|| Error::Extraction("missing slide relationship".into()))?;
                if let Some(text) = cache.get(relationship) {
                    total += text.len();
                    if total > MAX_XML as usize || pages.len() >= MAX_PAGES {
                        return Err(Error::Extraction("extracted text or page limit".into()));
                    }
                    pages.push(text.clone());
                    continue;
                }
                let target = id
                    .as_ref()
                    .and_then(|id| rels.get(id))
                    .ok_or_else(|| Error::Extraction("missing slide relationship".into()))?;
                let member = if target.starts_with('/') {
                    target.trim_start_matches('/').to_owned()
                } else {
                    format!("ppt/{target}")
                };
                if member.split('/').any(|p| p == "..") {
                    return Err(Error::Extraction("unsupported slide target".into()));
                }
                let mut text = xml_text(&read_zip(archive, &member, expanded)?)?;
                let (dir, name) = member
                    .rsplit_once('/')
                    .ok_or_else(|| Error::Extraction("invalid slide path".into()))?;
                let rel_path = format!("{dir}/_rels/{name}.rels");
                if archive.file_names().any(|n| n == rel_path) {
                    for target in
                        related_parts(&read_zip(archive, &rel_path, expanded)?, "notesSlide")?
                    {
                        let part = resolve_part(&member, &target)?;
                        text.push_str("\n## Speaker notes\n");
                        text.push_str(&xml_text(&read_zip(archive, &part, expanded)?)?);
                    }
                }
                total += text.len();
                if total > MAX_XML as usize {
                    return Err(Error::Extraction("extracted text exceeds 64 MiB".into()));
                }
                cache.insert(relationship.clone(), text.clone());
                pages.push(text);
                if pages.len() > MAX_PAGES {
                    return Err(Error::Extraction("page limit".into()));
                }
            }
            Event::Eof => break,
            _ => (),
        }
    }
    Ok(pages)
}

// Relationship targets are package-relative, never filesystem paths.
fn resolve_part(base: &str, target: &str) -> Result<String> {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        base.rsplit_once('/')
            .map(|(dir, _)| dir.split('/').collect())
            .unwrap_or_default()
    };
    for part in target.split('/') {
        match part {
            "" | "." => (),
            ".." => {
                if parts.pop().is_none() {
                    return Err(Error::Extraction("invalid package target".into()));
                }
            }
            p => parts.push(p),
        }
    }
    Ok(parts.join("/"))
}
fn related_parts(bytes: &[u8], kind: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_reader(bytes);
    let mut parts = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|e| Error::Extraction(e.to_string()))?
        {
            Event::Empty(e) | Event::Start(e) if e.local_name().as_ref() == b"Relationship" => {
                let mut target = None;
                let mut matches = false;
                let mut external = false;
                for a in e.attributes() {
                    let a = a.map_err(|e| Error::Extraction(e.to_string()))?;
                    let value = a
                        .unescape_value()
                        .map_err(|e| Error::Extraction(e.to_string()))?
                        .into_owned();
                    match a.key.as_ref() {
                        b"Target" => target = Some(value),
                        b"Type" => matches = value.rsplit('/').next() == Some(kind),
                        b"TargetMode" => external = value == "External",
                        _ => (),
                    }
                }
                if matches
                    && !external
                    && let Some(target) = target
                {
                    parts.push(target);
                }
            }
            Event::DocType(_) => {
                return Err(Error::Extraction(
                    "XML document types are unsupported".into(),
                ));
            }
            Event::Eof => break,
            _ => (),
        }
    }
    Ok(parts)
}
pub(super) fn docx(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    expanded: &mut usize,
) -> Result<Vec<String>> {
    let mut text = xml_text(&read_zip(archive, "word/document.xml", expanded)?)?;
    let rel = "word/_rels/document.xml.rels";
    if archive.file_names().any(|n| n == rel) {
        let bytes = read_zip(archive, rel, expanded)?;
        for (kind, heading) in [("header", "Header"), ("footer", "Footer")] {
            let mut seen = std::collections::BTreeSet::new();
            for target in related_parts(&bytes, kind)? {
                let part = resolve_part("word/document.xml", &target)?;
                if seen.insert(part.clone()) {
                    text.push_str(&format!("\n## {heading}\n"));
                    text.push_str(&xml_text(&read_zip(archive, &part, expanded)?)?);
                    if text.len() > MAX_XML as usize {
                        return Err(Error::Extraction("extracted text exceeds 64 MiB".into()));
                    }
                }
            }
        }
    }
    Ok(vec![text])
}
