//! PDF fallback extraction through external tools, and page geometry.
use super::{ToolDiscovery, Tools, process::tool};
use crate::{Error, Result, pins::PageGeometry};
use std::path::Path;
use std::process::{Command, Stdio};

pub(super) fn pdf_fallback(
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

pub(super) fn pdf_geometry(doc: &lopdf::Document, id: lopdf::ObjectId) -> Option<PageGeometry> {
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
