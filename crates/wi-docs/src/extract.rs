//! Local extractors. Subprocesses receive temporary snapshots, never originals.
use crate::{Error, Ocr, Result, Status, pins::PageGeometry};
use quick_xml::{Reader, events::Event};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use zip::ZipArchive;

const MAX_XML: u64 = 64 * 1024 * 1024;
const MAX_PAGES: usize = 100_000;

const TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const TOOL_MAX_STDOUT: usize = 64 * 1024 * 1024;
const IMPORT_BUDGET: Duration = Duration::from_secs(600);
const MAX_EXPANDED: usize = 256 * 1024 * 1024;

fn duration_env(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

// Nonblocking sockets avoid reader threads remaining blocked when a tool's
// descendants inherit stdout/stderr. Both streams and every poll are bounded.
#[cfg(unix)]
fn run_tool(cmd: &mut Command, timeout: Duration, max_stdout: usize) -> Result<Output> {
    use std::os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    };
    // Keep Error::Io reserved for spawn errors so only a failed spawn can
    // establish tool absence during discovery.
    fn tool_io(error: std::io::Error) -> Error {
        Error::Extraction(format!("tool I/O: {error}"))
    }
    fn drain(stream: &mut UnixStream, bytes: &mut Vec<u8>, cap: usize) -> Result<()> {
        let mut buffer = [0; 8192];
        // Bound each turn so a continuously writing child cannot defeat timeout.
        for _ in 0..32 {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if n > cap.saturating_sub(bytes.len()) {
                        return Err(Error::Extraction("output-too-large".into()));
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(tool_io(e)),
            }
        }
        Ok(())
    }
    if timeout.is_zero() {
        return Err(Error::Extraction("tool-timeout".into()));
    }
    let (mut stdout, child_stdout) = UnixStream::pair().map_err(tool_io)?;
    let (mut stderr, child_stderr) = UnixStream::pair().map_err(tool_io)?;
    stdout.set_nonblocking(true).map_err(tool_io)?;
    stderr.set_nonblocking(true).map_err(tool_io)?;
    let started = Instant::now();
    let mut child = cmd
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(child_stdout)))
        .stderr(Stdio::from(OwnedFd::from(child_stderr)))
        .spawn()?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = (|| {
        loop {
            drain(&mut stdout, &mut out, max_stdout)?;
            drain(&mut stderr, &mut err, max_stdout)?;
            if started.elapsed() >= timeout {
                return Err(Error::Extraction("tool-timeout".into()));
            }
            if let Some(status) = child.try_wait().map_err(tool_io)? {
                // Drain everything already queued; descendants cannot hold us open.
                loop {
                    let before = out.len() + err.len();
                    drain(&mut stdout, &mut out, max_stdout)?;
                    drain(&mut stderr, &mut err, max_stdout)?;
                    if before == out.len() + err.len() {
                        break;
                    }
                    if started.elapsed() >= timeout {
                        return Err(Error::Extraction("tool-timeout".into()));
                    }
                }
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            std::thread::sleep(
                Duration::from_millis(20).min(timeout.saturating_sub(started.elapsed())),
            );
        }
    })();
    if result.is_err() {
        // A wrapper may have forked children that inherited the output handles.
        // Kill the group before reaping its leader, then fall back to direct kill.
        let _ = Command::new("/bin/kill")
            .args(["-KILL", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(not(unix))]
fn run_tool(_cmd: &mut Command, _timeout: Duration, _max_stdout: usize) -> Result<Output> {
    Err(Error::Extraction(
        "unsupported-platform: bounded local tools require Unix".into(),
    ))
}

fn tool(cmd: &mut Command, deadline: Instant) -> Result<Output> {
    run_tool(
        cmd,
        duration_env("WI_TOOL_TIMEOUT_MS", TOOL_TIMEOUT)
            .min(deadline.saturating_duration_since(Instant::now())),
        TOOL_MAX_STDOUT,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WordBox {
    pub page: usize,
    pub text: String,
    pub bbox: [f64; 4],
    pub confidence: f64,
    pub unit: String,
}

pub(crate) struct Extracted {
    pub pages: Vec<String>,
    pub geometry: Vec<Option<PageGeometry>>,
    pub words: Vec<WordBox>,
    pub engine: String,
    pub version: String,
    pub status: Status,
    pub needs_ocr: Vec<usize>,
    pub diagnostics: Vec<String>,
    pub page_extractors: Vec<PageExtractor>,
    pub pages_not_indexed: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageExtractor {
    pub page: usize,
    pub engine: String,
    pub version: String,
}

/// Discovery failures are transient; only a spawn NotFound proves absence.
#[derive(Debug, Serialize)]
pub(crate) enum ToolDiscovery {
    Present(String),
    Absent,
    Unknown(String),
}
impl ToolDiscovery {
    pub fn as_deref(&self) -> Option<&str> {
        match self {
            Self::Present(version) => Some(version),
            Self::Absent | Self::Unknown(_) => None,
        }
    }
}

/// Installed tool identities are part of the options hash, so adding a fallback
/// does not silently reuse a run made without it.
#[derive(Debug, Serialize)]
pub(crate) struct Tools {
    pub pdftotext: ToolDiscovery,
    pub pdftoppm: ToolDiscovery,
    pub tesseract: ToolDiscovery,
    pub language: String,
    #[serde(skip)]
    deadline: Instant,
    #[serde(skip)]
    diagnostics: Vec<String>,
}
impl Tools {
    pub fn discover(pdf: bool, ocr: Ocr, started: Instant) -> Self {
        let deadline =
            started + duration_env("WI_IMPORT_BUDGET_MS", IMPORT_BUDGET).min(IMPORT_BUDGET);
        let mut diagnostics = Vec::new();
        let mut discover = |name, flag, enabled: bool| {
            if !enabled {
                return ToolDiscovery::Absent;
            }
            let result = tool_version(name, flag, deadline);
            if let ToolDiscovery::Unknown(cause) = &result {
                diagnostics.push(format!("{name} discovery: {cause}"));
            }
            result
        };
        let tesseract = discover("tesseract", "--version", ocr != Ocr::Off);
        let pdftotext = discover("pdftotext", "-v", pdf);
        let pdftoppm = discover("pdftoppm", "-v", pdf && ocr != Ocr::Off);
        let language = if tesseract.as_deref().is_some() {
            match tool(Command::new("tesseract").arg("--list-langs"), deadline) {
                Ok(out) if out.status.success() => {
                    if String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .any(|s| s.trim() == "kor")
                    {
                        "kor+eng"
                    } else {
                        "eng"
                    }
                }
                result => {
                    let cause = match result {
                        Ok(out) => format!("abnormal exit: {}", out.status),
                        Err(e) => e.to_string(),
                    };
                    diagnostics.push(format!("tesseract language discovery: {cause}"));
                    "eng"
                }
            }
        } else {
            "eng"
        }
        .to_owned();
        Self {
            pdftotext,
            pdftoppm,
            tesseract,
            language,
            deadline,
            diagnostics,
        }
    }

    pub fn failure_diagnostic(&self, message: &str) -> String {
        std::iter::once(message)
            .chain(self.diagnostics.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn retryable(&self) -> bool {
        !self.diagnostics.is_empty()
    }
}
fn tool_version(name: &str, flag: &str, deadline: Instant) -> ToolDiscovery {
    let out = match tool(Command::new(name).arg(flag).stdin(Stdio::null()), deadline) {
        Ok(out) => out,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return ToolDiscovery::Absent;
        }
        Err(e) => return ToolDiscovery::Unknown(e.to_string()),
    };
    if !out.status.success() {
        return ToolDiscovery::Unknown(format!("abnormal exit: {}", out.status));
    }
    let bytes = if out.stdout.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    };
    match String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .filter(|s| !s.trim().is_empty())
    {
        Some(version) => ToolDiscovery::Present(version.to_owned()),
        None => ToolDiscovery::Unknown("empty version output".into()),
    }
}

pub(crate) fn media(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "csv" => "text/csv",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "tif" | "tiff" => "image/tiff",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

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

fn pptx(
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
fn docx(archive: &mut ZipArchive<Cursor<&[u8]>>, expanded: &mut usize) -> Result<Vec<String>> {
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

fn pdf_fallback(
    snapshot: &Path,
    n: u32,
    tools: &Tools,
) -> Result<Option<(String, String, String)>> {
    if let ToolDiscovery::Present(version) = &tools.pdftotext {
        let out = tool(
            Command::new("pdftotext")
                .args(["-f", &n.to_string(), "-l", &n.to_string(), "-layout"])
                .arg(snapshot)
                .arg("-")
                .stdin(Stdio::null()),
            tools.deadline,
        )?;
        let text = String::from_utf8_lossy(&out.stdout).replace('\u{c}', "");
        if out.status.success() && !text.trim().is_empty() {
            return Ok(Some((text, "pdftotext".into(), version.clone())));
        }
    }
    if let (ToolDiscovery::Present(poppler), ToolDiscovery::Present(tesseract)) =
        (&tools.pdftoppm, &tools.tesseract)
    {
        let dir = tempfile::tempdir()?;
        let prefix = dir.path().join("page");
        let out = tool(
            Command::new("pdftoppm")
                .args([
                    "-f",
                    &n.to_string(),
                    "-l",
                    &n.to_string(),
                    "-singlefile",
                    "-scale-to",
                    "4096",
                    "-png",
                ])
                .arg(snapshot)
                .arg(&prefix)
                .stdin(Stdio::null()),
            tools.deadline,
        )?;
        if !out.status.success() {
            return Err(Error::Extraction("pdftoppm rasterization failed".into()));
        }
        let out = tool(
            Command::new("tesseract")
                .arg(prefix.with_extension("png"))
                .args(["stdout", "-l", &tools.language])
                .stdin(Stdio::null()),
            tools.deadline,
        )?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            return Err(Error::Extraction(format!(
                "tesseract failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        if !text.trim().is_empty() {
            return Ok(Some((
                text,
                "pdftoppm+tesseract".into(),
                format!("{poppler}; {tesseract}; {}", tools.language),
            )));
        }
    }
    Ok(None)
}

fn pdf_geometry(doc: &lopdf::Document, id: lopdf::ObjectId) -> Option<PageGeometry> {
    let page = doc.get_object(id).ok()?.as_dict().ok()?;
    if let Ok(unit) = page.get(b"UserUnit")
        && unit.as_float().ok()? != 1.0
    {
        return None;
    }
    let mut id = id;
    for _ in 0..100 {
        let dict = doc.get_object(id).ok()?.as_dict().ok()?;
        if let Ok(value) = dict.get(b"MediaBox") {
            let array = doc.dereference(value).ok()?.1.as_array().ok()?;
            let coords: Vec<f64> = array
                .iter()
                .filter_map(|v| v.as_float().ok().map(f64::from))
                .collect();
            if coords.len() == 4
                && coords.iter().all(|n| n.is_finite())
                && coords[2] > coords[0]
                && coords[3] > coords[1]
            {
                return Some(PageGeometry {
                    width: (coords[2] - coords[0]).abs(),
                    height: (coords[3] - coords[1]).abs(),
                    unit: "pdf_point".into(),
                });
            }
        }
        id = dict.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

pub(crate) fn extract(
    bytes: &[u8],
    ext: &str,
    ocr: Ocr,
    ocr_version: Option<&str>,
    tools: &Tools,
    max_text_bytes: usize,
) -> Result<Extracted> {
    let mut result = Extracted {
        pages: Vec::new(),
        geometry: Vec::new(),
        words: Vec::new(),
        engine: String::new(),
        version: String::new(),
        status: Status::Complete,
        needs_ocr: Vec::new(),
        diagnostics: tools.diagnostics.clone(),
        page_extractors: Vec::new(),
        pages_not_indexed: Vec::new(),
    };
    match ext {
        "pdf" => {
            result.engine = "lopdf".into();
            result.version = "0.38.0".into();
            let doc =
                lopdf::Document::load_mem(bytes).map_err(|e| Error::Extraction(e.to_string()))?;
            if doc.is_encrypted() {
                return Err(Error::Extraction("encrypted PDF is unsupported".into()));
            }
            let pages = doc.get_pages();
            if pages.len() > MAX_PAGES {
                return Err(Error::Extraction("page limit".into()));
            }
            let mut snapshot = tempfile::Builder::new().suffix(".pdf").tempfile()?;
            snapshot.write_all(bytes)?;
            snapshot.flush()?;
            let mut retained = 0;
            for (n, id) in pages {
                if Instant::now() >= tools.deadline {
                    let page = result.pages.len() + 1;
                    result.pages.push(String::new());
                    result.geometry.push(pdf_geometry(&doc, id));
                    result.pages_not_indexed.push(page);
                    result.page_extractors.push(PageExtractor {
                        page,
                        engine: "skipped".into(),
                        version: String::new(),
                    });
                    result.diagnostics.push(format!("page {n}: import-budget"));
                    continue;
                }
                let mut text = match doc.extract_text(&[n]) {
                    Ok(text) => text,
                    Err(e) => {
                        result.diagnostics.push(format!("page {n}: {e}"));
                        String::new()
                    }
                };
                let mut extractor = PageExtractor {
                    page: result.pages.len() + 1,
                    engine: "lopdf".into(),
                    version: "0.38.0".into(),
                };
                if text.trim().is_empty() {
                    match pdf_fallback(snapshot.path(), n, tools) {
                        Ok(Some((fallback, engine, version))) => {
                            text = fallback;
                            extractor.engine = engine;
                            extractor.version = version;
                        }
                        Ok(None) => (),
                        Err(e) => result.diagnostics.push(format!("page {n}: {e}")),
                    }
                }
                result.page_extractors.push(extractor);
                if text.trim().is_empty() {
                    result.needs_ocr.push(result.pages.len() + 1);
                }
                if !result.pages_not_indexed.is_empty() || retained + text.len() > max_text_bytes {
                    result.pages_not_indexed.push(result.pages.len() + 1);
                    text.clear();
                }
                retained += text.len();
                result.pages.push(text);
                result.geometry.push(pdf_geometry(&doc, id));
            }
            if !result.needs_ocr.is_empty() {
                result.diagnostics.push(
                    "blank or undecodable pages remain after available local PDF fallbacks".into(),
                );
            }
        }
        "docx" | "pptx" => {
            result.engine = "wi-ooxml".into();
            result.version = env!("CARGO_PKG_VERSION").into();
            let mut archive = ZipArchive::new(Cursor::new(bytes))
                .map_err(|e| Error::Extraction(e.to_string()))?;
            result.pages = if ext == "docx" {
                if Instant::now() >= tools.deadline {
                    result.pages_not_indexed.push(1);
                    result
                        .diagnostics
                        .push("DOCX skipped: import-budget".into());
                    vec![String::new()]
                } else {
                    docx(&mut archive, &mut 0)?
                }
            } else {
                pptx(
                    &mut archive,
                    &mut 0,
                    tools.deadline,
                    &mut result.pages_not_indexed,
                )?
            };
            if ext == "pptx" && !result.pages_not_indexed.is_empty() {
                result
                    .diagnostics
                    .push("remaining slides skipped: import-budget".into());
            }
            result.diagnostics.push(if ext == "docx" { "DOCX is one logical page; physical pagination, footnotes, and embedded media are not extracted" } else { "Slide text follows presentation order; charts and embedded media are not extracted" }.into());
        }
        "csv" => {
            result.engine = "csv".into();
            result.version = "1.4.0".into();
            let mut reader = csv::ReaderBuilder::new()
                .has_headers(false)
                .from_reader(bytes);
            // Stream records and stop retaining the logical page at its bound.
            let mut body = String::new();
            let mut markdown = false;
            for (i, record) in reader.records().enumerate() {
                if Instant::now() >= tools.deadline {
                    result.pages_not_indexed.push(1);
                    result.diagnostics.push("CSV skipped: import-budget".into());
                    body.clear();
                    break;
                }
                let record = record.map_err(|e| Error::Extraction(e.to_string()))?;
                if i == 0 {
                    markdown = record.len() <= 50;
                }
                let cells: Vec<String> = record
                    .iter()
                    .map(|cell| {
                        let cell = cell
                            .replace('\\', "\\\\")
                            .replace("\r\n", "\n")
                            .replace('\r', "\n")
                            .replace('\n', "\\n")
                            .replace('\t', "\\t");
                        if markdown {
                            cell.replace('|', "\\|")
                        } else {
                            cell
                        }
                    })
                    .collect();
                let row = if markdown {
                    format!("| {} |\n", cells.join(" | "))
                } else {
                    format!("{}\n", cells.join("\t"))
                };
                body.push_str(&row);
                if i == 0 && markdown {
                    body.push_str(&format!("| {} |\n", vec!["---"; cells.len()].join(" | ")));
                }
                if body.len() > max_text_bytes {
                    result.pages_not_indexed.push(1);
                    body.clear();
                    result.diagnostics.push("CSV parsing stopped at the logical-page size bound; remaining records were not validated".into());
                    break;
                }
            }
            result.pages.push(body);
        }
        _ => {
            result.engine = "imagesize".into();
            result.version = "0.14.0".into();
            let size = imagesize::blob_size(bytes).map_err(|e| Error::Extraction(e.to_string()))?;
            result.geometry.push(Some(PageGeometry {
                width: size.width as f64,
                height: size.height as f64,
                unit: "pixel".into(),
            }));
            result.pages.push(String::new());
            if ocr != Ocr::Off
                && let Some(version) = ocr_version
            {
                result.engine = "tesseract".into();
                result.version = version.into();
                match image_ocr(bytes, ext, &tools.language, tools.deadline) {
                    Ok((text, words))
                        if words.iter().all(|w| {
                            w.bbox[2] <= size.width as f64 && w.bbox[3] <= size.height as f64
                        }) =>
                    {
                        result.pages[0] = text;
                        result.words = words;
                    }
                    Ok(_) => result
                        .diagnostics
                        .push("OCR word box exceeds source dimensions".into()),
                    Err(e) => result.diagnostics.push(e.to_string()),
                }
            }
            if result.pages[0].trim().is_empty() {
                result.needs_ocr.push(1);
                result.diagnostics.push(
                    if ocr == Ocr::Off {
                        "OCR disabled"
                    } else if ocr_version.is_none() {
                        "local tesseract is unavailable"
                    } else {
                        "OCR produced no text"
                    }
                    .into(),
                );
            }
        }
    }
    if result.pages.is_empty() {
        return Err(Error::Extraction("document has no pages".into()));
    }
    result.geometry.resize(result.pages.len(), None);
    // Normalize line endings while preserving the physical
    // page partition (including empty first/last pages).
    for text in &mut result.pages {
        *text = text.replace("\r\n", "\n").replace('\r', "\n");
        text.retain(|c| c != '\0');
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
    }
    for text in &mut result.pages {
        *text = text.trim_start_matches('\n').into();
        if !text.is_empty() {
            break;
        }
    }
    for text in result.pages.iter_mut().rev() {
        *text = text.trim_end_matches('\n').into();
        if !text.is_empty() {
            text.push('\n');
            break;
        }
    }
    if result.page_extractors.is_empty() {
        result.page_extractors = (1..=result.pages.len())
            .map(|page| PageExtractor {
                page,
                engine: result.engine.clone(),
                version: result.version.clone(),
            })
            .collect();
    }
    let mut retained = 0;
    let mut omitted_suffix = false;
    let omitted_start = result
        .pages_not_indexed
        .first()
        .copied()
        .unwrap_or(usize::MAX);
    for (i, page) in result.pages.iter_mut().enumerate() {
        if omitted_suffix || i + 1 >= omitted_start || retained + page.len() > max_text_bytes {
            omitted_suffix = true;
            result.pages_not_indexed.push(i + 1);
            page.clear();
        }
        retained += page.len();
    }
    result.pages_not_indexed.sort_unstable();
    result.pages_not_indexed.dedup();
    if !result.needs_ocr.is_empty() {
        result.status = if result.needs_ocr.len() == result.pages.len() {
            Status::NeedsOcr
        } else {
            Status::Partial
        };
    }
    if !result.pages_not_indexed.is_empty() {
        result.status = Status::Partial;
    }
    Ok(result)
}

fn image_ocr(
    bytes: &[u8],
    ext: &str,
    language: &str,
    deadline: Instant,
) -> Result<(String, Vec<WordBox>)> {
    let mut snapshot = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile()?;
    snapshot.write_all(bytes)?;
    snapshot.flush()?;
    let out = tool(
        Command::new("tesseract")
            .arg(snapshot.path())
            .args(["stdout", "-l", language, "tsv"])
            .stdin(Stdio::null()),
        deadline,
    )?;
    if !out.status.success() {
        return Err(Error::Extraction(format!(
            "tesseract failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    parse_tsv(&out.stdout)
}

fn parse_tsv(bytes: &[u8]) -> Result<(String, Vec<WordBox>)> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .quoting(false)
        .from_reader(bytes);
    let mut text = String::new();
    let mut words = Vec::new();
    let mut previous_line = None;
    for row in reader.records() {
        let row = row.map_err(|e| Error::Extraction(e.to_string()))?;
        if row.get(0) != Some("5") {
            continue;
        }
        if row.get(1) != Some("1") {
            return Err(Error::Extraction(
                "multi-frame image OCR is unsupported".into(),
            ));
        }
        let word = row.get(11).unwrap_or("").trim();
        if word.is_empty() {
            continue;
        }
        let number = |i| -> Result<f64> {
            row.get(i)
                .ok_or_else(|| Error::Extraction("incomplete OCR TSV".into()))?
                .parse()
                .map_err(|_| Error::Extraction("invalid OCR coordinates".into()))
        };
        let [left, top, width, height] = [number(6)?, number(7)?, number(8)?, number(9)?];
        let confidence = number(10)?;
        if ![left, top, width, height, confidence]
            .iter()
            .all(|n| n.is_finite())
            || left < 0.0
            || top < 0.0
            || width < 0.0
            || height <= 0.0
            || width <= 0.0
            || !(0.0..=100.0).contains(&confidence)
        {
            return Err(Error::Extraction("invalid OCR rectangle".into()));
        }
        let line = row.iter().skip(1).take(4).collect::<Vec<_>>().join(":");
        if !text.is_empty() {
            text.push(if previous_line.as_ref() == Some(&line) {
                ' '
            } else {
                '\n'
            });
        }
        text.push_str(word);
        previous_line = Some(line);
        words.push(WordBox {
            page: 1,
            text: word.into(),
            bbox: [left, top, left + width, top + height],
            confidence,
            unit: "pixel".into(),
        });
    }
    Ok((text, words))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Document, Object, Stream, dictionary};

    #[cfg(unix)]
    #[test]
    fn local_tool_bounds_both_streams_and_ignores_inherited_handles() {
        for script in [
            "while :; do printf 1234567890; done",
            "while :; do printf 1234567890 >&2; done",
        ] {
            let error = run_tool(
                Command::new("/bin/sh").args(["-c", script]),
                Duration::from_secs(2),
                1024,
            )
            .unwrap_err();
            assert!(error.to_string().contains("output-too-large"));
        }
        let started = Instant::now();
        let output = run_tool(
            Command::new("/bin/sh").args(["-c", "sleep 1 & printf done"]),
            Duration::from_millis(200),
            1024,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(output.stdout, b"done");
        assert!(output.status.success());
    }

    #[cfg(unix)]
    #[test]
    fn local_tool_failure_kills_descendants_in_its_process_group() {
        for (script_tail, expected) in [
            ("wait", "tool-timeout"),
            ("while :; do printf 1234567890; done", "output-too-large"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let pid_file = temp.path().join("pid");
            let script = format!("/bin/sleep 30 & echo $! > \"$1\"; {script_tail}");
            let result = run_tool(
                Command::new("/bin/sh")
                    .args(["-c", &script, "sh"])
                    .arg(&pid_file),
                Duration::from_millis(300),
                1024,
            );
            assert!(result.unwrap_err().to_string().contains(expected));
            let pid = std::fs::read_to_string(pid_file).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let out = Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", pid.trim()])
                    .output()
                    .unwrap();
                let state = String::from_utf8_lossy(&out.stdout);
                if state.trim().is_empty() || state.trim().starts_with('Z') {
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = Command::new("/bin/kill")
                        .args(["-KILL", pid.trim()])
                        .status();
                    panic!("descendant still alive after {expected}: {pid} {state}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn repeated_pptx_relationships_decompress_slide_only_once() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let relationships =
            "<Relationships><Relationship Id=\"r1\" Target=\"slides/one.xml\"/></Relationships>";
        let presentation = format!(
            "<presentation><sldIdLst>{}</sldIdLst></presentation>",
            "<sldId r:id=\"r1\"/>".repeat(1000)
        );
        let slide = format!("<slide>{}<t>same slide</t></slide>", " ".repeat(8192));
        for (name, text) in [
            ("ppt/_rels/presentation.xml.rels", relationships),
            ("ppt/presentation.xml", presentation.as_str()),
            ("ppt/slides/one.xml", slide.as_str()),
        ] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(text.as_bytes()).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let mut archive = ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        let mut expanded = 0;
        let pages = pptx(
            &mut archive,
            &mut expanded,
            Instant::now() + IMPORT_BUDGET,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(pages.len(), 1000);
        assert!(pages.iter().all(|text| text == "same slide"));
        assert_eq!(
            expanded,
            relationships.len() + presentation.len() + slide.len()
        );
    }

    #[test]
    fn absent_pdf_tools_keep_unmet_required_ocr_honest() {
        let mut doc = Document::with_version("1.5");
        let pages = doc.new_object_id();
        let content = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
        let page = doc.add_object(dictionary! {"Type"=>"Page", "Parent"=>pages, "Contents"=>content, "MediaBox"=>vec![0.into(),0.into(),612.into(),792.into()]});
        doc.objects.insert(
            pages,
            Object::Dictionary(
                dictionary! {"Type"=>"Pages", "Kids"=>vec![page.into()], "Count"=>1},
            ),
        );
        let root = doc.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
        doc.trailer.set("Root", root);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let tools = Tools {
            pdftotext: ToolDiscovery::Absent,
            pdftoppm: ToolDiscovery::Absent,
            tesseract: ToolDiscovery::Absent,
            language: "eng".into(),
            deadline: Instant::now() + IMPORT_BUDGET,
            diagnostics: Vec::new(),
        };
        let result = extract(&bytes, "pdf", Ocr::Required, None, &tools, 8192).unwrap();
        assert_eq!(result.status, Status::NeedsOcr);
        assert_eq!(result.needs_ocr, [1]);
        assert_eq!(result.page_extractors[0].engine, "lopdf");
        assert!(result.diagnostics.iter().any(|s| s.contains("remain")));
    }

    #[test]
    fn ocr_tsv_preserves_lines_boxes_and_rejects_other_frames() {
        let header = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n";
        let rows = "5\t1\t1\t1\t1\t1\t10\t20\t30\t40\t95\tHELLO\n5\t1\t1\t1\t2\t1\t10\t70\t20\t30\t90\t결제\n";
        let (text, words) = parse_tsv(format!("{header}{rows}").as_bytes()).unwrap();
        assert_eq!(text, "HELLO\n결제");
        assert_eq!(words[0].bbox, [10.0, 20.0, 40.0, 60.0]);
        assert!(parse_tsv(format!("{header}{}", rows.replace("5\t1", "5\t2")).as_bytes()).is_err());
    }
}
