//! Image OCR through tesseract and TSV word-box parsing.
use super::{WordBox, process::tool};
use crate::{Error, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

pub(super) fn image_ocr(
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

pub(super) fn parse_tsv(bytes: &[u8]) -> Result<(String, Vec<WordBox>)> {
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
