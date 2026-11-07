//! Offline Notion Markdown/CSV exports. No source file is rewritten.
use crate::{
    Error, ImportReport, MAX_SOURCE_BYTES, Result, Status, current_note_bound, field, optional,
    parse, render, validate_source_path,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use wi_history::hash_bytes;
use wi_note::remove_key;
use wi_store::{IMPORTS_DIR, Ingress, MoveRequest, Vault, WriteRequest};

const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 64;
type Files = BTreeMap<String, Vec<u8>>;

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// Strip exactly one Notion identifier, preserving ordinary titles.
fn clean_component(name: &str) -> String {
    let (stem, ext) = name
        .rsplit_once('.')
        .filter(|(_, e)| matches!(*e, "md" | "csv"))
        .map_or((name, ""), |(s, e)| (s, e));
    let title = stem
        .rsplit_once(' ')
        .filter(|(t, id)| {
            !t.is_empty() && id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .map_or(stem, |(t, _)| t);
    if ext.is_empty() {
        title.to_owned()
    } else {
        format!("{title}.{ext}")
    }
}

fn notion_name(path: &str) -> bool {
    path.split('/').any(|part| clean_component(part) != part)
}

fn checked_path(path: &str) -> Result<&str> {
    validate_source_path(path)?;
    if path.contains(['[', ']', '#']) {
        return Err(invalid(
            "export names contain unsupported wiki-link delimiters",
        ));
    }
    if Path::new(path).is_absolute() || path.split('/').count() > MAX_DEPTH {
        return Err(invalid("unsafe or excessively deep export path"));
    }
    Ok(path)
}

fn bounded_read(mut reader: impl Read, budget: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(budget as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > budget {
        return Err(invalid("Notion export exceeds source safety cap"));
    }
    Ok(bytes)
}

fn insert(files: &mut Files, path: String, bytes: Vec<u8>, total: &mut usize) -> Result<()> {
    if files.len() >= MAX_ENTRIES || files.contains_key(&path) {
        return Err(invalid("duplicate export member or too many entries"));
    }
    *total = total
        .checked_add(bytes.len())
        .ok_or_else(|| invalid("export size overflow"))?;
    if *total as u64 > MAX_SOURCE_BYTES {
        return Err(invalid("Notion export exceeds source safety cap"));
    }
    files.insert(path, bytes);
    Ok(())
}

fn directory(
    source: &Path,
    rel: &str,
    files: &mut Files,
    total: &mut usize,
    entries_left: &mut usize,
) -> Result<()> {
    let base = source.join(rel);
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&base)? {
        if *entries_left == 0 {
            return Err(invalid("too many export members"));
        }
        *entries_left -= 1;
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("export names must be UTF-8"))?;
        let path = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        checked_path(&path)?;
        let kind = entry.file_type()?;
        if kind.is_symlink() || (!kind.is_dir() && !kind.is_file()) {
            return Err(invalid("export contains a symlink or non-regular member"));
        }
        if kind.is_dir() {
            directory(source, &path, files, total, entries_left)?;
        } else {
            let remaining = MAX_SOURCE_BYTES as usize - *total;
            if entry.metadata()?.len() > remaining as u64 {
                return Err(invalid("Notion export exceeds source safety cap"));
            }
            let bytes = bounded_read(std::fs::File::open(entry.path())?, remaining)?;
            insert(files, path, bytes, total)?;
        }
    }
    Ok(())
}

fn zip_preflight(bytes: &[u8]) -> Result<()> {
    fn u16_at(bytes: &[u8], offset: usize) -> Result<usize> {
        let raw: [u8; 2] = bytes
            .get(offset..offset + 2)
            .ok_or_else(|| invalid("truncated ZIP header"))?
            .try_into()
            .map_err(|_| invalid("ZIP header"))?;
        Ok(u16::from_le_bytes(raw) as usize)
    }
    fn u32_at(bytes: &[u8], offset: usize) -> Result<usize> {
        let raw: [u8; 4] = bytes
            .get(offset..offset + 4)
            .ok_or_else(|| invalid("truncated ZIP header"))?
            .try_into()
            .map_err(|_| invalid("ZIP header"))?;
        Ok(u32::from_le_bytes(raw) as usize)
    }
    let start = bytes.len().saturating_sub(65_557);
    let end = (start..bytes.len().saturating_sub(21))
        .rev()
        .find(|i| {
            bytes.get(*i..*i + 4) == Some(b"PK\x05\x06")
                && u16_at(bytes, *i + 20).is_ok_and(|n| *i + 22 + n == bytes.len())
        })
        .ok_or_else(|| invalid("ZIP end record is missing"))?;
    let count = u16_at(bytes, end + 10)?;
    let size = u32_at(bytes, end + 12)?;
    let mut position = u32_at(bytes, end + 16)?;
    if count == 0xffff || size == u32::MAX as usize || position == u32::MAX as usize {
        return Err(invalid("ZIP64 exports are unsupported"));
    }
    if u16_at(bytes, end + 4)? != 0
        || u16_at(bytes, end + 6)? != 0
        || u16_at(bytes, end + 8)? != count
        || count > MAX_ENTRIES
        || position.checked_add(size) != Some(end)
    {
        return Err(invalid("unsupported or inconsistent ZIP directory"));
    }
    let mut names = BTreeSet::new();
    for _ in 0..count {
        if bytes.get(position..position + 4) != Some(b"PK\x01\x02") {
            return Err(invalid("invalid ZIP central header"));
        }
        let name_len = u16_at(bytes, position + 28)?;
        let extra_len = u16_at(bytes, position + 30)?;
        let comment_len = u16_at(bytes, position + 32)?;
        let name_start = position + 46;
        let name_end = name_start + name_len;
        let name = bytes
            .get(name_start..name_end)
            .ok_or_else(|| invalid("truncated ZIP name"))?;
        if !names.insert(name) {
            return Err(invalid("duplicate ZIP member"));
        }
        position = name_end + extra_len + comment_len;
        if position > end {
            return Err(invalid("invalid ZIP central directory size"));
        }
    }
    if position != end {
        return Err(invalid("inconsistent ZIP member count"));
    }
    Ok(())
}

/// External exports are read-only sources; converted paths remain vault-relative.
fn source_path(root: &Path, source: &str) -> Result<PathBuf> {
    let path = Path::new(source);
    if path.is_absolute() {
        if path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return Err(invalid("unsafe export source path"));
        }
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(invalid("symlink export source"));
        }
        return Ok(std::fs::canonicalize(path)?);
    }
    checked_path(source)?;
    let mut full = root.to_path_buf();
    for part in source.split('/') {
        full.push(part);
        if std::fs::symlink_metadata(&full)?.file_type().is_symlink() {
            return Err(invalid("symlink source component"));
        }
    }
    Ok(full)
}

fn load(vault: &Vault, source: &str) -> Result<Files> {
    let full = source_path(vault.root(), source)?;
    let mut files = Files::new();
    let mut total = 0;
    if full.is_dir() {
        let mut entries_left = MAX_ENTRIES;
        directory(&full, "", &mut files, &mut total, &mut entries_left)?;
        return Ok(files);
    }
    if std::fs::metadata(&full)?.len() > MAX_SOURCE_BYTES {
        return Err(invalid("Notion ZIP exceeds source safety cap"));
    }
    let snapshot = bounded_read(std::fs::File::open(&full)?, MAX_SOURCE_BYTES as usize)?;
    let mut entries_left = MAX_ENTRIES;
    load_zip(&snapshot, 0, &mut files, &mut total, &mut entries_left)?;
    Ok(files)
}

fn part_zip(name: &str) -> bool {
    name.strip_suffix(".zip")
        .and_then(|n| n.rsplit_once("-Part-"))
        .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn load_zip(
    snapshot: &[u8],
    depth: usize,
    files: &mut Files,
    total: &mut usize,
    entries_left: &mut usize,
) -> Result<()> {
    if depth > 2 {
        return Err(invalid("nested ZIP depth exceeds safety cap"));
    }
    zip_preflight(snapshot)?;
    let mut archive =
        zip::ZipArchive::new(Cursor::new(snapshot)).map_err(|e| invalid(e.to_string()))?;
    if archive.len() > *entries_left {
        return Err(invalid("too many ZIP entries"));
    }
    *entries_left -= archive.len();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| invalid(e.to_string()))?;
        let name = entry.name().trim_end_matches('/').to_owned();
        checked_path(&name)?;
        if entry.unix_mode().is_some_and(|mode| {
            mode & 0o170000 != 0 && !matches!(mode & 0o170000, 0o100000 | 0o040000)
        }) {
            return Err(invalid("ZIP contains a non-regular member"));
        }
        if entry.is_dir() {
            continue;
        }
        let remaining = MAX_SOURCE_BYTES as usize - *total;
        if entry.size() > remaining as u64 {
            return Err(invalid("expanded ZIP exceeds source safety cap"));
        }
        let bytes = bounded_read(&mut entry, remaining)?;
        if part_zip(&name) {
            // Charge wrapper bytes as well as expanded content to the shared budget.
            *total += bytes.len();
            load_zip(&bytes, depth + 1, files, total, entries_left)?;
        } else {
            insert(files, name, bytes, total)?;
        }
    }
    Ok(())
}

/// Lightweight name-based detection for callers which expand directory inputs.
/// Does not follow symlinks. Import repeats all containment and size checks.
pub fn is_export(root: &Path, source: &str) -> bool {
    let Ok(full) = source_path(root, source) else {
        return false;
    };
    fn scan(path: &Path, depth: usize, left: &mut usize) -> bool {
        if depth > MAX_DEPTH || *left == 0 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return false;
        };
        for entry in entries.flatten() {
            if *left == 0 {
                return false;
            }
            *left -= 1;
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if (kind.is_file() || kind.is_dir())
                && entry.file_name().to_str().is_some_and(notion_name)
            {
                return true;
            }
            if kind.is_dir() && scan(&entry.path(), depth + 1, left) {
                return true;
            }
        }
        false
    }
    let mut remaining = MAX_ENTRIES;
    full.is_dir() && scan(&full, 0, &mut remaining)
}

fn clean_path(path: &str) -> String {
    path.split('/')
        .map(clean_component)
        .collect::<Vec<_>>()
        .join("/")
}
fn stem(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .strip_suffix(".md")
        .unwrap_or(path.rsplit('/').next().unwrap_or(path))
}
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.into()
    } else {
        format!("{dir}/{name}")
    }
}

fn allocate(path: &str, used: &mut BTreeSet<String>) -> String {
    let mut candidate = path.to_owned();
    let mut number = 2;
    while !used.insert(candidate.to_lowercase()) {
        candidate = format!("{} ({number}).md", path.strip_suffix(".md").unwrap_or(path));
        number += 1;
    }
    candidate
}

fn assign(
    entry: &str,
    desired: &str,
    paths: &mut BTreeMap<String, String>,
    used: &mut BTreeSet<String>,
) -> String {
    if let Some(output) = paths.get(entry) {
        return output.clone();
    }
    let output = allocate(desired, used);
    paths.insert(entry.to_owned(), output.clone());
    output
}

fn output_dir(
    entry: &str,
    prefix: &str,
    paths: &BTreeMap<String, String>,
    dirs: &mut BTreeMap<String, String>,
) -> String {
    if entry.is_empty() {
        return prefix.to_owned();
    }
    if let Some(output) = dirs.get(entry) {
        return output.clone();
    }
    let base = output_dir(parent(entry), prefix, paths, dirs);
    let associated = paths
        .get(&format!("{entry}.md"))
        .or_else(|| paths.get(&format!("{entry}.csv")));
    let desired = associated.map_or_else(
        || {
            join(
                &base,
                &clean_component(entry.rsplit('/').next().unwrap_or(entry)),
            )
        },
        |p| p.strip_suffix(".md").unwrap_or(p).to_owned(),
    );
    let mut output = desired.clone();
    let mut number = 2;
    while dirs.values().any(|p| p.eq_ignore_ascii_case(&output)) {
        output = format!("{desired} ({number})");
        number += 1;
    }
    dirs.insert(entry.to_owned(), output.clone());
    output
}

struct Page {
    entry: String,
    output: String,
    body: String,
    properties: Map<String, Value>,
}

fn text(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|e| invalid(e.to_string()))
}
fn pages(
    files: &Files,
    prefix: &str,
    paths: &mut BTreeMap<String, String>,
    dirs: &mut BTreeMap<String, String>,
) -> Result<(Vec<Page>, BTreeMap<String, String>)> {
    let mut used: BTreeSet<String> = paths.values().map(|p| p.to_lowercase()).collect();
    used.insert(format!("{prefix}/_import.md").to_lowercase());
    let mut pages = Vec::new();
    let mut targets = BTreeMap::new();
    // Allocate parent pages/databases before their child folders. Persist both
    // mappings so adding a title collision cannot relocate earlier children.
    for entry in files
        .keys()
        .filter(|p| p.ends_with(".md") || p.ends_with(".csv"))
    {
        let dir = output_dir(parent(entry), prefix, paths, dirs);
        let filename = clean_component(entry.rsplit('/').next().unwrap_or(entry));
        let filename = if let Some(stem) = filename.strip_suffix(".csv") {
            format!("{stem}.md")
        } else {
            filename
        };
        let output = assign(entry, &join(&dir, &filename), paths, &mut used);
        targets.insert(entry.clone(), output);
    }
    for (entry, bytes) in files.iter().filter(|(p, _)| p.ends_with(".md")) {
        let output = targets[entry].clone();
        pages.push(Page {
            entry: entry.clone(),
            output,
            body: text(bytes)?.to_owned(),
            properties: Map::new(),
        });
    }
    for (entry, bytes) in files.iter().filter(|(p, _)| p.ends_with(".csv")) {
        let mut csv = csv::ReaderBuilder::new().from_reader(bytes.as_slice());
        let headers = csv.headers().map_err(|e| invalid(e.to_string()))?.clone();
        if headers.is_empty() || headers.iter().collect::<BTreeSet<_>>().len() != headers.len() {
            return Err(invalid("CSV requires unique property names"));
        }
        let dbdir = entry.strip_suffix(".csv").unwrap_or(entry);
        let index = targets[entry].clone();
        let mut body = format!("# {}\n\n", stem(&index));
        let mut row_titles = BTreeMap::<String, usize>::new();
        for (row, record) in csv.records().enumerate() {
            if pages.len() >= MAX_ENTRIES {
                return Err(invalid("too many converted pages"));
            }
            let record = record.map_err(|e| invalid(e.to_string()))?;
            let title = record
                .get(0)
                .filter(|s| !s.trim().is_empty())
                .map_or_else(|| format!("Row {}", row + 1), str::to_owned);
            if title.contains(['/', '\\', '\n', '\r', '\0', '[', ']', '|', '#']) {
                return Err(invalid("unsupported CSV row title"));
            }
            validate_source_path(&format!("{title}.md"))?;
            let occurrence = row_titles.entry(title.clone()).or_default();
            *occurrence += 1;
            let row_entry = format!("{entry}#row:{title}:{occurrence}");
            let properties = headers
                .iter()
                .zip(record.iter())
                .map(|(k, v)| (k.to_owned(), json!(v)))
                .collect();
            let matching = pages
                .iter()
                .enumerate()
                .filter(|(_, p)| parent(&p.entry) == dbdir && stem(&clean_path(&p.entry)) == title)
                .map(|(i, _)| i)
                .collect::<Vec<_>>();
            if matching.len() > 1 {
                return Err(invalid("ambiguous CSV row page"));
            }
            let output = if let Some(i) = matching.first() {
                if !pages[*i].properties.is_empty() {
                    return Err(invalid("multiple CSV rows match the same page"));
                }
                pages[*i].properties = properties;
                pages[*i].output.clone()
            } else {
                let dir = output_dir(dbdir, prefix, paths, dirs);
                let output = assign(
                    &row_entry,
                    &join(&dir, &format!("{title}.md")),
                    paths,
                    &mut used,
                );
                pages.push(Page {
                    entry: row_entry,
                    output: output.clone(),
                    body: format!("# {title}\n"),
                    properties,
                });
                output
            };
            body.push_str(&format!("- [[{}|{}]]\n", output, title));
        }
        pages.push(Page {
            entry: entry.clone(),
            output: index,
            body,
            properties: Map::new(),
        });
    }
    Ok((pages, targets))
}

fn decode(input: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let raw = input.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            let hex = std::str::from_utf8(raw.get(i + 1..i + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).ok()
}
fn resolve(entry: &str, url: &str) -> Option<String> {
    let decoded = decode(url)?;
    if decoded.starts_with('/') || decoded.contains(['\\', ':', '\0']) {
        return None;
    }
    let mut parts: Vec<&str> = parent(entry).split('/').filter(|p| !p.is_empty()).collect();
    for part in decoded.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn link_end(input: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut escaped = false;
    for (i, ch) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '(' => depth += 1,
            ')' if depth == 0 => return Some(i),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

fn rewrite(
    body: &str,
    entry: &str,
    targets: &BTreeMap<String, String>,
    diagnostics: &mut BTreeSet<String>,
) -> String {
    let mut result = String::new();
    let mut fence: Option<(char, usize)> = None;
    let mut inline = 0;
    let mut comment = false;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed.chars().take_while(|c| *c == marker).count();
        if inline == 0 && !comment && matches!(marker, '`' | '~') && count >= 3 {
            if fence.is_some_and(|(m, n)| m == marker && count >= n) {
                fence = None;
            } else if fence.is_none() {
                fence = Some((marker, count));
            }
            result.push_str(line);
            continue;
        }
        if fence.is_some() || line.starts_with("    ") || line.starts_with('\t') {
            result.push_str(line);
            continue;
        }
        if inline == 0 && !comment && (trimmed.starts_with('[') && trimmed.contains("]:")) {
            diagnostics.insert("reference-style Markdown links retained".into());
        }
        let mut i = 0;
        let last_link = line.rfind("](");
        while i < line.len() {
            let tail = &line[i..];
            let ch = tail.chars().next().unwrap_or(' ');
            if comment {
                if let Some(end) = tail.find("-->") {
                    result.push_str(&tail[..end + 3]);
                    i += end + 3;
                    comment = false;
                    continue;
                }
                result.push_str(tail);
                break;
            }
            if inline == 0 && tail.starts_with("<!--") {
                result.push_str("<!--");
                i += 4;
                comment = true;
                continue;
            }
            if inline == 0 && tail.starts_with("][") {
                diagnostics.insert("reference-style Markdown links retained".into());
            }
            if ch == '\\' {
                let end =
                    i + ch.len_utf8() + line[i + 1..].chars().next().map_or(0, char::len_utf8);
                result.push_str(&line[i..end]);
                i = end;
                continue;
            }
            if ch == '`' {
                let n = tail.bytes().take_while(|b| *b == b'`').count();
                if inline == 0 {
                    inline = n;
                } else if inline == n {
                    inline = 0;
                }
                result.push_str(&tail[..n]);
                i += n;
                continue;
            }
            if ch == '['
                && inline == 0
                && last_link.is_some_and(|last| i < last)
                && let Some(close) = tail.find("](")
            {
                let Some(end) = link_end(&tail[close + 2..]) else {
                    diagnostics.insert("unclosed Markdown link retained".into());
                    result.push_str(tail);
                    break;
                };
                let end = close + 2 + end;
                let label = &tail[1..close];
                let url = &tail[close + 2..end];
                let (path, fragment) = url.split_once('#').map_or((url, ""), |(a, b)| (a, b));
                if let Some(resolved) = resolve(entry, path) {
                    if let Some(output) = targets.get(&resolved) {
                        if label.contains(['[', ']', '|']) || fragment.contains(['[', ']', '|']) {
                            diagnostics
                                .insert("unsupported Markdown label or fragment retained".into());
                        } else {
                            let name = output.as_str();
                            let suffix = if fragment.is_empty() {
                                String::new()
                            } else {
                                format!("#{fragment}")
                            };
                            result.push_str(&format!("[[{name}{suffix}|{label}]]"));
                            i += end + 1;
                            continue;
                        }
                    } else if !path.is_empty() {
                        diagnostics.insert(format!("unresolved export link: {url}"));
                    }
                }
                result.push_str(&tail[..end + 1]);
                i += end + 1;
                continue;
            }
            result.push(ch);
            i += ch.len_utf8();
        }
    }
    result
}

fn editable_hash(bytes: &[u8]) -> Result<String> {
    let mut note = parse(bytes)?;
    for key in ["origin", "imported_at", "notion_checksum"] {
        note = remove_key(&note, key).map_err(|e| invalid(e.to_string()))?;
    }
    Ok(hash_bytes(note.to_bytes()))
}

/// A non-page file of the export, copied beside the converted pages.
struct Attachment {
    entry: String,
    output: String,
    checksum: String,
    bytes: Vec<u8>,
    /// `None`: already current. `Some(None)`: create. `Some(Some(c))`: replace `c`.
    expected: Option<Option<String>>,
}

/// Plan attachment copies. Paths stay stable across imports through the
/// manifest; a copy changed in the vault since the last import is a conflict.
fn attachments(
    vault: &Vault,
    files: &Files,
    prefix: &str,
    paths: &BTreeMap<String, String>,
    dirs: &mut BTreeMap<String, String>,
    manifest_path: &str,
) -> Result<Vec<Attachment>> {
    let recorded: BTreeMap<String, (String, String)> = match optional(vault, manifest_path)? {
        Some(old) => field(&parse(&old.bytes)?, "properties")
            .and_then(|v| v.get("attachments").cloned())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
            .into_iter()
            .map(|(entry, value)| {
                let path = value.get("path").and_then(Value::as_str);
                let checksum = value.get("checksum").and_then(Value::as_str);
                match (path, checksum) {
                    (Some(path), Some(checksum))
                        if checked_path(path).is_ok()
                            && path.starts_with(&format!("{prefix}/"))
                            && !path.ends_with(".md") =>
                    {
                        Ok((entry, (path.to_owned(), checksum.to_owned())))
                    }
                    _ => Err(Error::Conflict(manifest_path.to_owned())),
                }
            })
            .collect::<Result<_>>()?,
        None => BTreeMap::new(),
    };
    let mut used: BTreeSet<String> = paths
        .values()
        .chain(recorded.values().map(|(p, _)| p))
        .map(|p| p.to_lowercase())
        .collect();
    let mut planned = Vec::new();
    for (entry, bytes) in files
        .iter()
        .filter(|(p, _)| !p.ends_with(".md") && !p.ends_with(".csv"))
    {
        let output = if let Some((path, _)) = recorded.get(entry) {
            path.clone()
        } else {
            let dir = output_dir(parent(entry), prefix, paths, dirs);
            let name = entry.rsplit('/').next().unwrap_or(entry);
            let (stem, ext) = name
                .rsplit_once('.')
                .filter(|(s, _)| !s.is_empty())
                .map_or((name, String::new()), |(s, e)| (s, format!(".{e}")));
            let mut candidate = join(&dir, name);
            let mut number = 2;
            while !used.insert(candidate.to_lowercase()) {
                candidate = join(&dir, &format!("{stem} ({number}){ext}"));
                number += 1;
            }
            checked_path(&candidate)?;
            candidate
        };
        let checksum = hash_bytes(bytes);
        let current = match vault.read(&output) {
            Ok(read) => Some(read.checksum),
            Err(wi_store::StoreError::NotFound { .. }) => None,
            Err(e) => return Err(e.into()),
        };
        let expected = match current {
            Some(current) if current == checksum => None,
            Some(current) if recorded.get(entry).is_some_and(|(_, c)| *c == current) => {
                Some(Some(current))
            }
            Some(_) => return Err(Error::Conflict(output)),
            None => Some(None),
        };
        planned.push(Attachment {
            entry: entry.clone(),
            output,
            checksum,
            bytes: bytes.clone(),
            expected,
        });
    }
    Ok(planned)
}

/// Where an import into `prefix` keeps its record (mapping and checksums):
/// in the hidden imports folder, never among the converted pages.
fn record_path(prefix: &str) -> String {
    format!("{IMPORTS_DIR}/{prefix}.md")
}

/// The visible record older versions kept beside the converted pages.
fn visible_record_path(prefix: &str) -> String {
    format!("{prefix}/_Import.md")
}

/// The record of an earlier import into `prefix`, hidden or still visible.
fn existing_record(vault: &Vault, prefix: &str) -> Result<Option<wi_store::ReadResult>> {
    match optional(vault, &record_path(prefix))? {
        Some(record) => Ok(Some(record)),
        None => optional(vault, &visible_record_path(prefix)),
    }
}

/// Move a record an older version kept among the notes into the hidden
/// imports folder, once. An edited record is a conflict, as before; a
/// visible copy that reappears after the move is left to the owner.
fn adopt_visible_record(vault: &Vault, prefix: &str) -> Result<()> {
    let visible = visible_record_path(prefix);
    let hidden = record_path(prefix);
    let Some(old) = optional(vault, &visible)? else {
        return Ok(());
    };
    if optional(vault, &hidden)?.is_some() {
        return Ok(());
    }
    if field(&parse(&old.bytes)?, "notion_checksum") != Some(json!(editable_hash(&old.bytes)?)) {
        return Err(Error::Conflict(visible));
    }
    let moved = vault.move_import_record(MoveRequest {
        from: visible,
        to: hidden.clone(),
        expected_checksum: Some(old.checksum),
        op_id: None,
        ingress: Ingress::Import,
    })?;
    // A move keeps the old address as a permalink; a hidden record needs
    // none, and its checksum covers the bytes it was written with.
    if moved.checksum != hash_bytes(&old.bytes) {
        vault.write_import_record(WriteRequest {
            path: hidden,
            content: old.bytes,
            expected_checksum: Some(moved.checksum),
            op_id: None,
            ingress: Ingress::Import,
        })?;
    }
    Ok(())
}

/// Import a validated export through the existing note write path.
pub(crate) fn import(vault: &Vault, source: &str) -> Result<ImportReport> {
    let mut files = load(vault, source)?;
    let mut diagnostics = BTreeSet::new();
    files.retain(|path, _| {
        let protected = path
            .split('/')
            .any(|part| matches!(part, ".wiki-island" | ".git"));
        if protected {
            diagnostics.insert(format!("refused protected export member: {path}"));
        }
        !protected
    });
    if !files.keys().any(|p| notion_name(p)) {
        return Err(invalid(
            "not a Notion Markdown/CSV export (no page identifiers)",
        ));
    }
    let source_hash = hash_bytes(&serde_json::to_vec(
        &files
            .iter()
            .map(|(p, b)| (p, hash_bytes(b)))
            .collect::<BTreeMap<_, _>>(),
    )?);
    let id = hash_bytes(source.as_bytes());
    let legacy_prefix = format!("notion-imports/{}", &id[7..19]);
    let export_name = Path::new(source)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Export");
    checked_path(export_name)?;
    let source_name = Path::new(source)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(export_name);
    // Existing imports keep their paths; new imports get a readable folder.
    let prefix = if existing_record(vault, &legacy_prefix)?.is_some() {
        legacy_prefix
    } else {
        let base = format!("Notion/{export_name}");
        let mut candidate = base.clone();
        let mut number = 2;
        loop {
            match existing_record(vault, &candidate)? {
                None => break candidate,
                Some(old) => {
                    let note = parse(&old.bytes)?;
                    if field(&note, "properties").and_then(|p| p.get("source_id").cloned())
                        == Some(json!(id))
                    {
                        break candidate;
                    }
                    candidate = format!("{base} ({number})");
                    number += 1;
                    if number > MAX_ENTRIES {
                        return Err(invalid("too many import folder collisions"));
                    }
                }
            }
        }
    };
    adopt_visible_record(vault, &prefix)?;
    let manifest_path = record_path(&prefix);
    let mut paths: BTreeMap<String, String> = if let Some(old) = optional(vault, &manifest_path)? {
        let note = parse(&old.bytes)?;
        if !matches!(field(&note, "notion_export"), Some(v) if v == json!(source) || v == json!(source_name))
            || field(&note, "notion_checksum") != Some(json!(editable_hash(&old.bytes)?))
        {
            return Err(Error::Conflict(manifest_path));
        }
        serde_json::from_value(
            field(&note, "properties")
                .and_then(|v| v.get("paths").cloned())
                .ok_or_else(|| Error::Conflict(manifest_path.clone()))?,
        )?
    } else {
        BTreeMap::new()
    };
    let mut prior_outputs = BTreeSet::new();
    for output in paths.values() {
        checked_path(output)?;
        if !output.starts_with(&format!("{prefix}/"))
            || !output.ends_with(".md")
            || !prior_outputs.insert(output.to_lowercase())
        {
            return Err(Error::Conflict(manifest_path.clone()));
        }
    }
    let mut dirs: BTreeMap<String, String> = if let Some(old) = optional(vault, &manifest_path)? {
        serde_json::from_value(
            field(&parse(&old.bytes)?, "properties")
                .and_then(|v| v.get("directories").cloned())
                .unwrap_or_else(|| json!({})),
        )?
    } else {
        BTreeMap::new()
    };
    let mut directory_outputs = BTreeSet::new();
    for output in dirs.values() {
        checked_path(output)?;
        if !output.starts_with(&format!("{prefix}/"))
            || !directory_outputs.insert(output.to_lowercase())
        {
            return Err(Error::Conflict(manifest_path.clone()));
        }
    }
    let (mut pages, mut targets) = pages(&files, &prefix, &mut paths, &mut dirs)?;
    if pages.is_empty() {
        return Err(invalid(
            "Notion export contains no Markdown pages or CSV databases",
        ));
    }
    let attachments = attachments(vault, &files, &prefix, &paths, &mut dirs, &manifest_path)?;
    for attachment in &attachments {
        targets.insert(attachment.entry.clone(), attachment.output.clone());
    }
    let page_count = pages.len();
    let manifest_body = format!(
        "# Notion import\n\n{}",
        pages
            .iter()
            .map(|p| format!("- [[{}]]\n", p.output))
            .collect::<String>()
    );
    pages.push(Page {
        entry: "manifest".into(),
        output: manifest_path.clone(),
        body: manifest_body,
        properties: serde_json::from_value(
            json!({"source_id":id,"paths":paths, "directories":dirs, "attachments":attachments.iter().map(|a| (a.entry.clone(), json!({"path":a.output, "checksum":a.checksum}))).collect::<Map<_, _>>(), "source_checksum":source_hash}),
        )?,
    });
    let bound = current_note_bound(vault)?;
    let mut writes = Vec::new();
    let mut total = 0usize;
    for page in &mut pages {
        let body = rewrite(&page.body, &page.entry, &targets, &mut diagnostics);
        let existing = optional(vault, &page.output)?;
        let mut fields = Map::new();
        fields.insert("type".into(), json!("note"));
        fields.insert("notion_entry".into(), json!(page.entry));
        if page.entry == "manifest" {
            fields.insert("notion_export".into(), json!(source_name));
            fields.insert("properties".into(), json!(page.properties));
        } else {
            for (key, value) in &page.properties {
                let reserved = wi_note::ENGINE_OWNED_FIELDS.contains(&key.as_str())
                    || matches!(
                        key.as_str(),
                        "type"
                            | "kind"
                            | "claim_kind"
                            | "sources"
                            | "applicability"
                            | "excluded"
                            | "pinned"
                            | "noted_by"
                            | "noted_at"
                            | "properties"
                            | "notion_entry"
                            | "notion_export"
                            | "notion_checksum"
                            | "imported_at"
                    );
                let mut output = key.clone();
                if reserved {
                    output = format!("notion_{output}");
                    while page.properties.contains_key(&output) || fields.contains_key(&output) {
                        output = format!("notion_{output}");
                    }
                }
                fields.insert(output, value.clone());
            }
        }
        let fields = Value::Object(fields);
        let base = render(fields, &body)?;
        let checksum = hash_bytes(base.as_bytes());
        let mut note = parse(base.as_bytes())?;
        if let Some(old) = &existing {
            let old_note = parse(&old.bytes)?;
            if field(&old_note, "notion_entry") != Some(json!(page.entry))
                || field(&old_note, "notion_checksum") != Some(json!(editable_hash(&old.bytes)?))
            {
                return Err(Error::Conflict(page.output.clone()));
            }
            if field(&old_note, "notion_checksum") == Some(json!(checksum)) {
                continue;
            }
        }
        note = wi_note::set_key(&note, "notion_checksum", &json!(checksum))
            .map_err(|e| invalid(e.to_string()))?;
        // Creation adds store-owned provenance; replacements carry the old
        // origin. Include those bytes in the configured indexability bound.
        let origin = if let Some(old) = &existing {
            field(&parse(&old.bytes)?, "origin")
        } else {
            Some(
                json!({"kind":"file", "locator":"import:write", "captured_at":"2000-01-01T00:00:00Z"}),
            )
        };
        let stored_size = if let Some(value) = origin {
            wi_note::set_key(&note, "origin", &value)
                .map_err(|e| invalid(e.to_string()))?
                .to_bytes()
                .len()
        } else {
            note.to_bytes().len()
        };
        let content = note.to_bytes().to_vec();
        if stored_size > bound {
            return Err(invalid(format!(
                "{} exceeds configured note bound",
                page.output
            )));
        }
        total = total
            .checked_add(stored_size)
            .ok_or_else(|| invalid("output size overflow"))?;
        if total as u64 > MAX_SOURCE_BYTES {
            return Err(invalid("converted export exceeds safety cap"));
        }
        writes.push(WriteRequest {
            path: page.output.clone(),
            content,
            expected_checksum: existing.map(|v| v.checksum),
            op_id: None,
            ingress: Ingress::Import,
        });
    }
    let unchanged = writes.is_empty() && attachments.iter().all(|a| a.expected.is_none());
    // Attachments first, so a page never links to a file that is not there yet.
    for attachment in attachments {
        if let Some(expected) = attachment.expected {
            vault.write_attachment(
                &attachment.output,
                attachment.bytes,
                expected,
                Ingress::Import,
            )?;
        }
    }
    for request in writes {
        if request.path == manifest_path {
            vault.write_import_record(request)?;
        } else {
            vault.write(request)?;
        }
    }
    Ok(ImportReport {
        sidecar_path: prefix,
        run_note_path: manifest_path,
        source_sha256: source_hash,
        pages: page_count,
        status: if diagnostics.is_empty() {
            Status::Complete
        } else {
            Status::Partial
        },
        diagnostics: diagnostics.into_iter().collect(),
        unchanged,
    })
}
