//! Local extractors. Subprocesses receive temporary snapshots, never originals.
use crate::{Error, Ocr, Result, Status, pins::PageGeometry};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Write};
use std::time::{Duration, Instant};
use zip::ZipArchive;

const MAX_XML: u64 = 64 * 1024 * 1024;
const MAX_PAGES: usize = 100_000;

const TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const TOOL_MAX_STDOUT: usize = 64 * 1024 * 1024;
const IMPORT_BUDGET: Duration = Duration::from_secs(600);
const MAX_EXPANDED: usize = 256 * 1024 * 1024;

mod ocr;
mod office;
mod pdf;
mod process;

use ocr::image_ocr;
use office::{docx, pptx};
use pdf::{pdf_fallback, pdf_geometry};

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

#[cfg(test)]
mod tests {
    use super::ocr::parse_tsv;
    #[cfg(any(unix, windows))]
    use super::process::run_tool;
    use super::*;
    use lopdf::{Document, Object, Stream, dictionary};
    use std::process::Command;

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
    fn local_tool_failure_leaves_unrelated_process_groups_alive() {
        use std::os::unix::process::CommandExt;

        let mut unrelated = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let result = run_tool(
            Command::new("/bin/sleep").arg("30"),
            Duration::from_millis(100),
            1024,
        );
        let survived = unrelated.try_wait().unwrap().is_none();
        let _ = unrelated.kill();
        let _ = unrelated.wait();

        assert!(result.unwrap_err().to_string().contains("tool-timeout"));
        assert!(
            survived,
            "tool termination reached an unrelated process group"
        );
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

    #[cfg(windows)]
    #[test]
    fn local_windows_tool_returns_both_streams_and_bounds_output() {
        let output = run_tool(
            Command::new("cmd").args(["/D", "/C", "echo done & echo warning 1>&2"]),
            Duration::from_secs(5),
            1024,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "done");
        assert_eq!(String::from_utf8(output.stderr).unwrap().trim(), "warning");
        for redirect in ["", "1>&2"] {
            let script = format!("for /L %i in (1,1,100000) do @echo 1234567890 {redirect}");
            let error = run_tool(
                Command::new("cmd").args(["/D", "/C", &script]),
                Duration::from_secs(10),
                1024,
            )
            .unwrap_err();
            assert!(error.to_string().contains("output-too-large"), "{error}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn local_windows_tool_timeout_and_leader_exit_kill_grandchildren() {
        use wi_sys::process::{Liveness, liveness};
        for tail in ["$p.WaitForExit()", ""] {
            let temp = tempfile::tempdir().unwrap();
            let pid_path = temp.path().join("pid");
            let script = format!(
                "$p=Start-Process ping -ArgumentList '-n','30','127.0.0.1' -NoNewWindow -PassThru; [IO.File]::WriteAllText($env:WI_TOOL_PID,[string]$p.Id); {tail}"
            );
            let result = run_tool(
                Command::new("cmd")
                    .args(["/D", "/C", "powershell", "-NoProfile", "-Command", &script])
                    .env("WI_TOOL_PID", &pid_path),
                Duration::from_secs(5),
                8192,
            );
            if tail.is_empty() {
                assert!(result.unwrap().status.success());
            } else {
                assert!(result.unwrap_err().to_string().contains("tool-timeout"));
            }
            let pid = std::fs::read_to_string(pid_path).unwrap().parse().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while liveness(pid) != Liveness::Gone && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(liveness(pid), Liveness::Gone);
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
