//! Model-free local document import. Source files remain canonical and unchanged.
#![forbid(unsafe_code)]
mod extract;
pub mod notion;
pub mod pins;
pub use extract::{PageExtractor, WordBox};
use pins::{PageGeometry, PageMap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Instant;
use uuid::Uuid;
use wi_history::hash_bytes;
/// Shared vault ignore policy for source discovery.
pub use wi_index::ignored;
use wi_note::{Note, set_key};
use wi_store::{Ingress, StoreError, Vault, WriteRequest};

pub type Result<T> = std::result::Result<T, Error>;
const PIPELINE: &str = "wi-docs/2";
/// Unmeasured source safety policy, checked before loading through the store.
pub const MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;
const PROFILE: &str = "local-text-v1";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid document request: {0}")]
    Invalid(String),
    #[error("extraction failed: {0}")]
    Extraction(String),
    #[error("document import conflict: {0}; existing bytes were preserved")]
    Conflict(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ocr {
    #[default]
    Auto,
    Off,
    Required,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ImportOptions {
    pub ocr: Ocr,
    pub bulk: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Complete,
    Partial,
    NeedsOcr,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub sidecar_path: String,
    pub run_note_path: String,
    pub source_sha256: String,
    pub pages: usize,
    pub status: Status,
    pub diagnostics: Vec<String>,
    pub unchanged: bool,
}

/// Additional provenance lives in the run note's fenced JSON body, keeping the
/// document frontmatter and page-map schema stable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPayload {
    pub utf8_page_map: PageMap,
    pub geometry: Vec<Option<PageGeometry>>,
    pub words: Vec<WordBox>,
    pub diagnostics: Vec<String>,
    #[serde(default)]
    pub page_extractors: Vec<PageExtractor>,
    #[serde(default)]
    pub pages_not_indexed: Vec<usize>,
    /// Exact accepted sidecar, allowing recovery after a run-only publication.
    pub sidecar: String,
}

fn namespace() -> Uuid {
    // Persisted namespace: keep document and run identities stable across imports.
    Uuid::from_u128(0x9e04480b3bc057f49ee76f4f183e11bc)
}

pub fn document_id(source: Uuid) -> Uuid {
    Uuid::new_v5(&namespace(), format!("document-note:{source}").as_bytes())
}

/// The serialized identity uses sorted ASCII keys and no whitespace.
pub fn run_id(
    source: Uuid,
    checksum: &str,
    engine: &str,
    version: &str,
    options_hash: &str,
) -> Uuid {
    let identity = json!({"extraction_options_hash": options_hash, "extractor_engine": engine.trim(),
        "extractor_version": version.trim(), "extraction_profile": PROFILE, "pipeline_version": PIPELINE,
        "prompt_version": null, "source_checksum": checksum, "source_entity_external_id": source});
    let canonical: String = identity
        .to_string()
        .chars()
        .map(|c| {
            if c.is_ascii() {
                c.to_string()
            } else {
                let mut units = [0; 2];
                c.encode_utf16(&mut units)
                    .iter()
                    .map(|u| format!("\\u{u:04x}"))
                    .collect::<String>()
            }
        })
        .collect();
    Uuid::new_v5(&namespace(), canonical.as_bytes())
}

/// The extraction summary forbids extra keys and its `engine` is part
/// of the pre-extraction run identity, so fallback extractors actually used
/// (and any OCR) are recorded as schema-valid tags; per-page detail is in the
/// run note's `page_extractors`.
fn sidecar_tags(primary: &str, pages: &[PageExtractor]) -> Vec<String> {
    let mut tags = vec!["document".to_string(), "generated".to_string()];
    for page in pages.iter().filter(|p| p.engine != primary) {
        let slug: String = page
            .engine
            .to_ascii_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let tag = format!("extractor/{slug}");
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    if pages.iter().any(|p| p.engine.contains("tesseract")) {
        tags.push("ocr".into());
    }
    tags
}

fn field(note: &Note, key: &str) -> Option<Value> {
    note.frontmatter().and_then(|f| f.get(key))
}
fn parse(bytes: &[u8]) -> Result<Note> {
    Note::parse(bytes).map_err(|e| Error::Invalid(e.to_string()))
}

fn render(fields: Value, body: &str) -> Result<String> {
    // Start from a known empty block: extracted content may itself start with ---.
    let mut note = parse(format!("---\n---\n{body}").as_bytes())?;
    for (key, value) in fields
        .as_object()
        .ok_or_else(|| Error::Invalid("frontmatter must be an object".into()))?
    {
        note = set_key(&note, key, value).map_err(|e| Error::Invalid(e.to_string()))?;
    }
    String::from_utf8(note.to_bytes().to_vec()).map_err(|e| Error::Invalid(e.to_string()))
}

fn optional(vault: &Vault, path: &str) -> Result<Option<wi_store::ReadResult>> {
    match vault.read(path) {
        Ok(value) if value.diagnostics.is_empty() => Ok(Some(value)),
        Ok(_) => Err(Error::Conflict(format!("{path} changed during read"))),
        Err(StoreError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn read_run(bytes: &[u8]) -> Result<RunPayload> {
    let note = parse(bytes)?;
    let body = note.body();
    let json = body
        .strip_prefix("```json\n")
        .and_then(|s| s.strip_suffix("\n```\n"))
        .ok_or_else(|| Error::Invalid("unsupported run payload".into()))?;
    Ok(serde_json::from_str(json)?)
}

// Resource limits can change between identical imports, so retry bounded failures.
// Definitive tool availability and versions are captured in the options hash.
fn transient_diagnostic(message: &str) -> bool {
    [
        "tool-timeout",
        "import-budget",
        "output-too-large",
        "discovery:",
    ]
    .iter()
    .any(|cause| message.contains(cause))
}

fn retryable_run(bytes: &[u8]) -> Result<bool> {
    let note = parse(bytes)?;
    if let Some(failure) = field(&note, "failure") {
        return Ok(
            failure.get("retryable").and_then(Value::as_bool) == Some(true)
                || transient_diagnostic(&failure.to_string()),
        );
    }
    Ok(read_run(bytes)?
        .diagnostics
        .iter()
        .any(|d| transient_diagnostic(d)))
}

fn validate_prior(vault: &Vault, sidecar_path: &str, bytes: &[u8]) -> Result<bool> {
    let note = parse(bytes)?;
    let ingestion =
        field(&note, "ingestion").ok_or_else(|| Error::Conflict(sidecar_path.into()))?;
    let id = ingestion
        .get("run_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Error::Conflict(sidecar_path.into()))?;
    let prior_path = format!("document-ingestion-runs/{id}.md");
    let prior = optional(vault, &prior_path)?
        .ok_or_else(|| Error::Conflict(format!("{sidecar_path}: prior run is missing")))?;
    let run = parse(&prior.bytes)?;
    let output = field(&run, "output").ok_or_else(|| Error::Conflict(prior_path.clone()))?;
    if output.pointer("/raw/checksum").and_then(Value::as_str) != Some(hash_bytes(bytes).as_str()) {
        return Err(Error::Conflict(format!(
            "{sidecar_path} was edited since its extraction"
        )));
    }
    let payload = read_run(&prior.bytes)?;
    if payload.sidecar.as_bytes() != bytes {
        return Err(Error::Conflict(format!(
            "{prior_path}: raw revision mismatch"
        )));
    }
    retryable_run(&prior.bytes)
}

/// Import a vault-relative document or a relative/absolute export. Does not flush: CLI flushes once
/// after all files, and callers may group imports with `import_many`.
pub fn import(vault: &Vault, rel_path: &str, options: ImportOptions) -> Result<ImportReport> {
    if options.bulk {
        return vault.import_batch(|| import_inner(vault, rel_path, options))?;
    }
    import_inner(vault, rel_path, options)
}

/// All successful writes in this invocation become one history commit, including
/// originals, run notes and sidecars. Per-file failures are returned individually.
pub fn import_many(
    vault: &Vault,
    paths: &[String],
    options: ImportOptions,
) -> Result<Vec<Result<ImportReport>>> {
    vault
        .import_batch(|| {
            paths
                .iter()
                .map(|p| import_inner(vault, p, options))
                .collect()
        })
        .map_err(Error::from)
}

fn validate_source_path(path: &str) -> Result<()> {
    if path.trim() != path
        || path.contains(['\n', '\r', '\0', '\\', '<', '>', ':', '"', '|', '?', '*'])
    {
        return Err(Error::Invalid("source path is not portable".into()));
    }
    for part in path.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            return Err(Error::Invalid("source path is not portable".into()));
        }
    }
    Ok(())
}

fn current_note_bound(vault: &Vault) -> Result<usize> {
    // The index reloads its size policy on each sync; do not use only the
    // VaultConfig cached when this long-lived Vault was opened.
    let read = vault.read(wi_note::VAULT_CONFIG_PATH)?;
    if !read.diagnostics.is_empty() {
        return Err(Error::Conflict("configuration changed during read".into()));
    }
    let text = std::str::from_utf8(&read.bytes).map_err(|e| Error::Invalid(e.to_string()))?;
    let config = wi_note::VaultConfig::parse(text).map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(config.max_note_bytes().min(usize::MAX as u64) as usize)
}

// Normalize native separators only at the caller boundary. Archive member names
// still pass the portable path validator without normalization.
fn source_input(path: &str) -> std::borrow::Cow<'_, str> {
    #[cfg(windows)]
    if !Path::new(path).is_absolute() {
        return std::borrow::Cow::Owned(path.replace('\\', "/"));
    }
    std::borrow::Cow::Borrowed(path)
}

fn import_inner(vault: &Vault, rel_path: &str, options: ImportOptions) -> Result<ImportReport> {
    let input = source_input(rel_path);
    let rel_path = input.as_ref();
    if Path::new(rel_path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
        || notion::is_export(vault.root(), rel_path)
    {
        return notion::import(vault, rel_path);
    }
    let started = Instant::now();
    // Validate components before the size preflight; store.read remains the
    // authoritative contained read and the checksum is rechecked before publish.
    validate_source_path(rel_path)?;
    if Path::new(rel_path).is_absolute() {
        return Err(Error::Invalid("source must be vault-relative".into()));
    }
    let mut source_path = vault.root().to_path_buf();
    for part in rel_path.split('/') {
        source_path.push(part);
        if std::fs::symlink_metadata(&source_path)?
            .file_type()
            .is_symlink()
        {
            return Err(Error::Invalid("symlink source component".into()));
        }
    }
    if std::fs::metadata(&source_path)?.len() > MAX_SOURCE_BYTES {
        return Err(Error::Invalid(format!(
            "source exceeds unmeasured {MAX_SOURCE_BYTES}-byte safety cap"
        )));
    }
    let source_read = vault.read(rel_path)?;
    if source_read.bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(Error::Invalid(
            "source grew beyond safety cap during read".into(),
        ));
    }
    let max_note_bytes = current_note_bound(vault)?;
    if !source_read.diagnostics.is_empty() {
        return Err(Error::Conflict("source changed during read".into()));
    }
    let rel_path = source_read.path.as_str();
    validate_source_path(rel_path)?;
    let ext = Path::new(rel_path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let media = extract::media(&ext)
        .ok_or_else(|| Error::Invalid(format!("unsupported document extension: {ext}")))?;
    let sidecar_path = format!("{rel_path}.md");
    let existing = optional(vault, &sidecar_path)?;
    let retry_existing = if let Some(prior) = &existing {
        validate_prior(vault, &sidecar_path, &prior.bytes)?
    } else {
        false
    };
    let vault_id = Uuid::parse_str(vault.vault_id()).map_err(|e| Error::Invalid(e.to_string()))?;
    let source_id = Uuid::new_v5(&vault_id, format!("file:{rel_path}").as_bytes());
    let tools = extract::Tools::discover(
        ext == "pdf",
        if ext == "pdf" || media.starts_with("image/") {
            options.ocr
        } else {
            Ocr::Off
        },
        started,
    );
    let ocr_version = tools.tesseract.as_deref();
    let options_hash = hash_bytes(json!({"ocr": options.ocr, "tools": tools, "max_note_bytes": max_note_bytes, "format": ext, "profile": PROFILE}).to_string().as_bytes());
    let (engine, version) = match ext.as_str() {
        "pdf" => ("lopdf", "0.38.0"),
        "docx" | "pptx" => ("wi-ooxml", env!("CARGO_PKG_VERSION")),
        "csv" => ("csv", "1.4.0"),
        _ => match ocr_version {
            Some(v) => ("tesseract", v),
            None => ("imagesize", "0.14.0"),
        },
    };
    let id = run_id(
        source_id,
        &source_read.checksum,
        engine,
        version,
        &options_hash,
    );
    let run_note_path = format!("document-ingestion-runs/{id}.md");
    let prior_run = optional(vault, &run_note_path)?;
    let retry = tools.retryable()
        || retry_existing
        || prior_run
            .as_ref()
            .map(|r| retryable_run(&r.bytes))
            .transpose()?
            .unwrap_or(false);
    if let Some(run_read) = prior_run.as_ref().filter(|_| !retry) {
        let run = parse(&run_read.bytes)?;
        if let Some(failure) = field(&run, "failure") {
            return Ok(ImportReport {
                sidecar_path,
                run_note_path,
                source_sha256: source_read.checksum,
                pages: 0,
                status: Status::Failed,
                diagnostics: vec![
                    failure
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("prior extraction failed")
                        .into(),
                ],
                unchanged: true,
            });
        }
        let payload = read_run(&run_read.bytes)?;
        if run_read.bytes.len().max(payload.sidecar.len()) > max_note_bytes {
            return Err(Error::Conflict(
                "stored extraction exceeds current note bound".into(),
            ));
        }
        let sidecar = parse(payload.sidecar.as_bytes())?;
        let extraction =
            field(&run, "extraction").ok_or_else(|| Error::Conflict(run_note_path.clone()))?;
        let expected_id = field(&sidecar, "ingestion").and_then(|v| v.get("run_id").cloned());
        let source = field(&sidecar, "source");
        let raw_checksum = field(&run, "output").and_then(|v| v.pointer("/raw/checksum").cloned());
        if expected_id != Some(json!(id))
            || source.as_ref().and_then(|s| s.get("checksum")) != Some(&json!(source_read.checksum))
            || raw_checksum != Some(json!(hash_bytes(payload.sidecar.as_bytes())))
        {
            return Err(Error::Conflict(format!(
                "{run_note_path}: inconsistent provenance"
            )));
        }
        let source_expected = json!({"kind":"file", "media_type":media,"entity_external_id":source_id,"file_path":rel_path,"checksum":source_read.checksum,"size_bytes":source_read.bytes.len()});
        let mut summary = extraction.clone();
        if let Some(object) = summary.as_object_mut() {
            object.remove("page_map");
        }
        let unicode_map: PageMap =
            serde_json::from_value(extraction.get("page_map").cloned().unwrap_or(Value::Null))?;
        unicode_map.validate(sidecar.body())?;
        if source != Some(source_expected.clone())
            || field(&run, "source") != Some(source_expected)
            || field(&sidecar, "extraction") != Some(summary)
            || extraction.get("engine") != Some(&json!(engine))
            || extraction.get("engine_version") != Some(&json!(version))
            || extraction.get("options_hash") != Some(&json!(options_hash))
            || field(&run, "output").and_then(|v| v.get("document_file_path").cloned())
                != Some(json!(sidecar_path))
            || field(&run, "output").and_then(|v| v.get("document_entity_external_id").cloned())
                != Some(json!(document_id(source_id)))
        {
            return Err(Error::Conflict(format!(
                "{run_note_path}: inconsistent source or extraction record"
            )));
        }
        payload.utf8_page_map.validate(sidecar.body())?;
        vault.ingest_import_source(rel_path, &source_read.checksum)?;
        let unchanged = existing
            .as_ref()
            .is_some_and(|old| old.bytes == payload.sidecar.as_bytes());
        if !unchanged {
            vault.lifecycle_write(WriteRequest {
                path: sidecar_path.clone(),
                content: payload.sidecar.into_bytes(),
                expected_checksum: existing.as_ref().map(|old| old.checksum.clone()),
                op_id: None,
                ingress: Ingress::Import,
            })?;
        }
        return Ok(ImportReport {
            sidecar_path,
            run_note_path,
            source_sha256: source_read.checksum,
            pages: extraction
                .get("page_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            status: serde_json::from_value(
                extraction.get("status").cloned().unwrap_or(Value::Null),
            )?,
            diagnostics: payload.diagnostics,
            unchanged,
        });
    }
    let extracted = match extract::extract(
        &source_read.bytes,
        &ext,
        options.ocr,
        ocr_version,
        &tools,
        max_note_bytes,
    ) {
        Ok(extracted) => extracted,
        Err(e) => {
            let message = tools.failure_diagnostic(&e.to_string());
            if retry && existing.is_some() {
                // Keep the raw snapshot and its run consistent if a retry fails
                // before producing a replacement (for example, temporary I/O).
                // The retained transient run remains eligible for the next retry.
                return Ok(ImportReport {
                    sidecar_path,
                    run_note_path,
                    source_sha256: source_read.checksum,
                    pages: 0,
                    status: Status::Failed,
                    diagnostics: vec![message],
                    unchanged: false,
                });
            }
            return failed_run(
                vault,
                &sidecar_path,
                &run_note_path,
                json!({"kind":"file","media_type":media,"entity_external_id":source_id,"file_path":source_read.path,"checksum":source_read.checksum,"size_bytes":source_read.bytes.len()}),
                &source_read,
                &message,
                (
                    prior_run.as_ref().map(|r| r.checksum.clone()),
                    json!({"engine":engine,"engine_version":version,"profile":PROFILE,"options_hash":options_hash,"status":"failed",
                    "extracted_at":chrono::Utc::now().to_rfc3339(),"duration_ms":started.elapsed().as_millis() as u64,
                    "page_count":0,"extracted_page_count":0,"requires_ocr":false,"ocr_page_count":0,"pages_needing_ocr":[]}),
                ),
            );
        }
    };
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let source = json!({"kind":"file", "media_type":media,"entity_external_id":source_id,"file_path":rel_path,"checksum":source_read.checksum,"size_bytes":source_read.bytes.len()});
    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    // Bound both complete serialized notes, including the escaped raw snapshot
    // and page metadata. Binary search retains a prefix of whole pages.
    let build = |keep: usize| -> Result<(String, String, Status, Vec<String>)> {
        let mut pages = extracted.pages.clone();
        let mut omitted: std::collections::BTreeSet<usize> =
            extracted.pages_not_indexed.iter().copied().collect();
        for (i, page) in pages.iter_mut().enumerate().skip(keep) {
            omitted.insert(i + 1);
            page.clear();
        }

        let status = if omitted.is_empty() {
            extracted.status
        } else {
            Status::Partial
        };
        let mut diagnostics = extracted.diagnostics.clone();
        if !omitted.is_empty() {
            diagnostics.push(format!("whole pages omitted to fit the configured {max_note_bytes}-byte index note bound; see pages_not_indexed"));
        }
        let (body, unicode_map) = PageMap::from_pages(&pages, false);
        let (_, byte_map) = PageMap::from_pages(&pages, true);
        let extraction = json!({"engine":extracted.engine,"engine_version":extracted.version,"profile":PROFILE,"options_hash":options_hash,
            "status":status,"extracted_at":timestamp,"duration_ms":duration_ms,
            "page_count":pages.len(),"extracted_page_count":(1..=pages.len()).filter(|p| !omitted.contains(p) && !extracted.needs_ocr.contains(p)).count(),
            "requires_ocr":!extracted.needs_ocr.is_empty(),"ocr_page_count":extracted.needs_ocr.len(),"pages_needing_ocr":extracted.needs_ocr});
        let ingestion = json!({"stage":"raw","pipeline_version":PIPELINE,"run_id":id,"input_checksum":source_read.checksum});
        let sidecar = render(
            json!({"schema_version":"1","schema":"schema/document-extraction","title":Path::new(rel_path).file_name().and_then(|s|s.to_str()).unwrap_or(rel_path),
            "type":"extracted_text","tags":sidecar_tags(&extracted.engine, &extracted.page_extractors),"source":source,"extraction":extraction,"ingestion":ingestion,"document":{},"parse_semantics":false}),
            &body,
        )?;
        let mut full_extraction = extraction;
        full_extraction["page_map"] = serde_json::to_value(unicode_map)?;
        let payload = RunPayload {
            utf8_page_map: byte_map,
            geometry: extracted.geometry.clone(),
            words: extracted
                .words
                .iter()
                .filter(|word| !omitted.contains(&word.page))
                .cloned()
                .collect(),
            diagnostics: diagnostics.clone(),
            page_extractors: extracted.page_extractors.clone(),
            pages_not_indexed: omitted.into_iter().collect(),
            sidecar: sidecar.clone(),
        };
        let run_body = format!("```json\n{}\n```\n", serde_json::to_string(&payload)?);
        let run = render(
            json!({"schema_version":"1","schema":"schema/document-ingestion-run","title":id,"type":"document_ingestion_run","tags":["document","ingestion-run"],
            "source":source,"extraction":full_extraction,"ingestion":{"run_id":id,"stage":"raw","pipeline_version":PIPELINE,"input_checksum":source_read.checksum,"started_at":timestamp},
            "output":{"document_entity_external_id":document_id(source_id),"document_file_path":sidecar_path,"raw":{"kind":"raw_extraction","checksum":hash_bytes(sidecar.as_bytes()),"created_at":timestamp}},"parse_semantics":false}),
            &run_body,
        )?;
        Ok((sidecar, run, status, diagnostics))
    };
    let fits = |notes: &(String, String, Status, Vec<String>)| {
        notes.0.len().max(notes.1.len()) <= max_note_bytes
    };
    let mut notes = build(extracted.pages.len())?;
    if !fits(&notes) {
        let mut low = 0;
        let mut high = extracted.pages.len();
        notes = build(0)?;
        if !fits(&notes) {
            return Err(Error::Invalid(format!(
                "extraction metadata alone exceeds configured {max_note_bytes}-byte note bound; no derived notes published"
            )));
        }
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            let candidate = build(mid)?;
            if fits(&candidate) {
                low = mid;
                notes = candidate;
            } else {
                high = mid - 1;
            }
        }
    }
    let (sidecar, run, status, diagnostics) = notes;
    // Extraction no longer needs the snapshot. Release it before the source
    // recheck and history ingestion read the original again.
    drop(source_read.bytes);
    if current_note_bound(vault)? != max_note_bytes {
        return Err(Error::Conflict(
            "configured note bound changed during extraction".into(),
        ));
    }
    // Detect changes made while extracting before publishing either derived note.
    let current_source = vault.read(rel_path)?;
    if current_source.checksum != source_read.checksum || !current_source.diagnostics.is_empty() {
        return Err(Error::Conflict("source changed during extraction".into()));
    }
    drop(current_source);
    vault.ingest_import_source(rel_path, &source_read.checksum)?;
    vault.lifecycle_write(WriteRequest {
        path: run_note_path.clone(),
        content: run.into_bytes(),
        expected_checksum: prior_run.as_ref().map(|r| r.checksum.clone()),
        op_id: None,
        ingress: Ingress::Import,
    })?;
    vault.lifecycle_write(WriteRequest {
        path: sidecar_path.clone(),
        content: sidecar.into_bytes(),
        expected_checksum: existing.map(|r| r.checksum),
        op_id: None,
        ingress: Ingress::Import,
    })?;
    Ok(ImportReport {
        sidecar_path,
        run_note_path,
        source_sha256: source_read.checksum,
        pages: extracted.pages.len(),
        status,
        diagnostics,
        unchanged: false,
    })
}

fn failed_run(
    vault: &Vault,
    sidecar: &str,
    run_path: &str,
    source_contract: Value,
    source: &wi_store::ReadResult,
    message: &str,
    extraction: (Option<String>, Value),
) -> Result<ImportReport> {
    let (expected_checksum, extraction) = extraction;
    let id = Path::new(run_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let now = chrono::Utc::now().to_rfc3339();
    let run = render(
        json!({"schema_version":"1","schema":"schema/document-ingestion-run","title":id,"type":"document_ingestion_run","source":source_contract,
        "ingestion":{"run_id":id,"stage":"failed","pipeline_version":PIPELINE,"input_checksum":source.checksum,"started_at":now,"completed_at":now},
        "extraction":extraction,"failure":{"code":"extraction_failed","message":message,"retryable":transient_diagnostic(message)},"parse_semantics":false}),
        "Extraction failed; the source was preserved.\n",
    )?;
    if run.len() > current_note_bound(vault)? {
        return Err(Error::Invalid(
            "failed-run metadata exceeds configured note bound".into(),
        ));
    }
    vault.lifecycle_write(WriteRequest {
        path: run_path.into(),
        content: run.into_bytes(),
        expected_checksum,
        op_id: None,
        ingress: Ingress::Import,
    })?;
    Ok(ImportReport {
        sidecar_path: sidecar.into(),
        run_note_path: run_path.into(),
        source_sha256: source.checksum.clone(),
        pages: 0,
        status: Status::Failed,
        diagnostics: vec![message.into()],
        unchanged: false,
    })
}

/// Document locator shape used by FORMAT page and region pins.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Locator {
    Page(pins::PagePin),
    Region(pins::RegionPin),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedDocumentPin {
    #[serde(rename = "ref")]
    pub source_path: String,
    pub source_hash: String,
    pub sidecar_path: String,
    pub sidecar_checksum: String,
    pub extracted_span_hash: String,
    pub resolved: pins::ResolvedPin,
}

/// Resolve a locator through the sidecar's accepted run, verifying both the
/// original file checksum and the raw extraction body. Refuses stale originals
/// or edited sidecars. Source bytes are read through the contained store path.
pub fn resolve_pin(
    vault: &Vault,
    sidecar_path: &str,
    locator: &Locator,
) -> Result<ResolvedDocumentPin> {
    let current = optional(vault, sidecar_path)?
        .ok_or_else(|| Error::Invalid("sidecar is missing".into()))?;
    validate_prior(vault, sidecar_path, &current.bytes)?;
    let note = parse(&current.bytes)?;
    let source =
        field(&note, "source").ok_or_else(|| Error::Invalid("source record is missing".into()))?;
    let path = source
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("source path is missing".into()))?;
    let source_hash = source
        .get("checksum")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("source checksum is missing".into()))?;
    if current.path != format!("{path}.md") {
        return Err(Error::Conflict(
            "sidecar is not at its source-derived path".into(),
        ));
    }
    let original = vault.read(path)?;
    if original.checksum != source_hash || !original.diagnostics.is_empty() {
        return Err(Error::Conflict(
            "original differs from the extraction source checksum".into(),
        ));
    }
    let ingestion = field(&note, "ingestion")
        .ok_or_else(|| Error::Invalid("ingestion record is missing".into()))?;
    let id = ingestion
        .get("run_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Error::Invalid("run identity is missing".into()))?;
    let run = optional(vault, &format!("document-ingestion-runs/{id}.md"))?
        .ok_or_else(|| Error::Conflict("run is missing".into()))?;
    let payload = read_run(&run.bytes)?;
    let requested_page = match locator {
        Locator::Page(pin) => pin.n,
        Locator::Region(pin) => pin.page,
    };
    if payload.pages_not_indexed.contains(&requested_page) {
        return Err(Error::Invalid(
            "page was omitted by the index note size bound".into(),
        ));
    }
    let resolved = match locator {
        Locator::Page(pin) => pins::resolve_page(note.body(), &payload.utf8_page_map, pin)?,
        Locator::Region(pin) => {
            pins::resolve_region(note.body(), &payload.utf8_page_map, &payload.geometry, pin)?
        }
    };
    let span = &note.body().as_bytes()[resolved.bytes.start..resolved.bytes.end];
    Ok(ResolvedDocumentPin {
        source_path: path.into(),
        source_hash: source_hash.into(),
        sidecar_path: current.path,
        sidecar_checksum: current.checksum,
        extracted_span_hash: hash_bytes(span),
        resolved,
    })
}

#[cfg(test)]
mod retry_tests {
    use super::transient_diagnostic;

    #[test]
    fn only_bounded_resource_failures_are_transient() {
        for diagnostic in ["tool-timeout", "import-budget", "output-too-large"] {
            assert!(transient_diagnostic(diagnostic), "{diagnostic}");
        }
        for diagnostic in [
            "local tesseract is unavailable",
            "pdftotext missing-tool",
            "pdftoppm spawn: No such file or directory",
            "i/o: Permission denied",
        ] {
            assert!(!transient_diagnostic(diagnostic), "{diagnostic}");
        }
    }
}
