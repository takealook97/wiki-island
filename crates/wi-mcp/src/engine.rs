use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::provider::{DeliveryHit, EVIDENCE_LABEL, Lifecycle, LifecycleError, ReadMode};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use wi_note::{LifecycleType, Note, generate_permalink};
use wi_store::{
    BodyEdit, EditRequest, Ingress, MoveRequest, Query, ReadResult, SearchHit, SearchMode,
    SearchOptions, StoreError, Vault, WriteRequest, WriteResult,
};

use crate::{dependencies, result, schema, window};

mod listing;
mod receipts;
mod search;

type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("not found")]
    NotFound,
}
impl Error {
    fn code(&self) -> &str {
        match self {
            Self::Store(e) => e.code(),
            Self::Lifecycle(e) => e.code(),
            Self::Invalid(_) => "invalid-request",
            Self::Unsupported(_) => "unsupported",
            Self::NotFound => "not-found",
        }
    }
}
fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str)
}
fn required<'a>(a: &'a Value, k: &str) -> Result<&'a str> {
    s(a, k).ok_or(Error::Invalid("required string is missing"))
}
fn owned(a: &Value, k: &str) -> Option<String> {
    s(a, k).map(str::to_owned)
}
fn n(a: &Value, k: &str, default: usize) -> usize {
    a.get(k)
        .and_then(schema::integer)
        .map_or(default, |n| n as usize)
}
fn b(a: &Value, k: &str, default: bool) -> bool {
    a.get(k).and_then(Value::as_bool).unwrap_or(default)
}
fn mode(a: &Value) -> ReadMode {
    match s(a, "read_mode") {
        Some("all" | "historic" | "explore") => ReadMode::All,
        _ => ReadMode::Current,
    }
}

/// Synchronous service. Own it on one thread; MCP's async adapter communicates
/// through a channel because the underlying gix Vault is not Send.
pub struct Engine {
    vault: Vault,
    lifecycle: Arc<dyn Lifecycle>,
    foreground_catch_ups: AtomicUsize,
}
impl Engine {
    pub fn new(vault: Vault, lifecycle: Arc<dyn Lifecycle>) -> Self {
        Self {
            vault,
            lifecycle,
            foreground_catch_ups: AtomicUsize::new(0),
        }
    }
    pub fn vault(&self) -> &Vault {
        &self.vault
    }
    /// Number of foreground catch-ups this engine has run (test and
    /// diagnostic hook; `new` and `initialize` never run one).
    pub fn foreground_catch_ups(&self) -> usize {
        self.foreground_catch_ups.load(Ordering::SeqCst)
    }

    /// Build the lifecycle read snapshot ahead of the first read, so it
    /// overlaps the client's handshake instead of the first tool call. It
    /// judges no note, reads no note body and never runs a catch-up.
    pub fn warm(&self) {
        let _ = self.vault.classify(&[], &Value::Null);
    }

    /// Queries that rank over the index need a complete walk. When the index
    /// has none (fresh device cache, vault created before the walk was
    /// recorded), run one foreground catch-up before the first such call. A
    /// failed catch-up is not remembered, so the next call retries and the
    /// store's degraded fallback still answers meanwhile.
    fn ensure_complete_walk(&self, name: &str) {
        if !matches!(name, "wi_search" | "wi_list" | "wi_find" | "wi_context") {
            return;
        }
        if matches!(self.vault.index_as_of(), Ok(Some(_))) {
            return;
        }
        self.foreground_catch_ups.fetch_add(1, Ordering::SeqCst);
        if self.vault.catch_up().is_err() {
            eprintln!("wi mcp: foreground index catch-up failed");
        }
    }

    pub fn call(&self, name: &str, mut args: Value) -> CallToolResult {
        let definition = schema::tools().into_iter().find(|t| t.name == name);
        let normalized = schema::normalize(name, &mut args);
        let valid = normalized
            && definition
                .as_ref()
                .is_some_and(|t| schema::validate(&json!(t.input_schema), &args));
        let generation = self.vault.generation().ok();
        if !valid || serde_json::to_vec(&args).map_or(true, |a| a.len() > 1024 * 1024) {
            let mut out = result::envelope(0, None, false);
            out["generation"] = json!(generation);
            out["index"] = Value::Null;
            out["error"] = json!({"code":if definition.is_none() {"unknown-tool"} else {"invalid-request"},"message":"Arguments do not match the tool schema or exceed 1 MiB."});
            return result::bounded(out, true);
        }
        self.ensure_complete_walk(name);
        let read = matches!(
            name,
            "wi_read" | "wi_search" | "wi_context" | "wi_list" | "wi_find" | "wi_history"
        );
        for attempt in 0..2 {
            let start = match self.vault.publication_mark() {
                Ok(m) => m,
                Err(_) => return generation_unavailable(),
            };
            let mut out = result::envelope(
                start.generation(),
                None,
                !self.vault.index_pending().is_empty(),
            );
            match self.dispatch(name, &args, &mut out) {
                Ok(()) => {
                    let end = match self.vault.publication_mark() {
                        Ok(m) => m,
                        Err(_) => return generation_unavailable(),
                    };
                    if read
                        && start != end
                        && dependencies::touched(&self.vault, name, &start, &out)
                    {
                        if attempt == 0 {
                            continue;
                        }
                        let count = out["hits"].as_array().map_or(0, Vec::len);
                        out["hits"] = json!([]);
                        out["items"] = json!([]);
                        out["edges"] = json!([]);
                        result::withhold(&mut out, "generation-changed", count);
                        out["diagnostics"] = json!(["generation-changed"]);
                    }
                    out["generation"] = json!(end.generation());
                    return result::bounded(out, false);
                }
                Err(e) => {
                    // Stable code + bounded human explanation. Never echo raw
                    // file bytes or arbitrary request strings in diagnostics.
                    let message = match &e {
                        Error::Invalid(m) | Error::Unsupported(m) => *m,
                        Error::Store(StoreError::Stale { .. }) => {
                            "Checksum mismatch. Read the note again."
                        }
                        Error::Store(StoreError::Exists { .. }) => {
                            "Note exists. Supply its expected_checksum to replace it."
                        }
                        Error::Store(StoreError::ClaimEditRequiresCandidate { .. }) => {
                            "Save the revised claim as a new note linked to the previous claim."
                        }
                        Error::Lifecycle(LifecycleError::Store(StoreError::BudgetExhausted {
                            ..
                        })) => "The automatic-change cap is spent; the change is queued.",
                        Error::Store(e) | Error::Lifecycle(LifecycleError::Store(e))
                            if e.is_query_too_long() =>
                        {
                            "The search text is too long. Search for a few key words instead."
                        }

                        _ => message(e.code()),
                    };
                    out["error"] = json!({"code":e.code(),"message":message});
                    if let Error::Store(StoreError::LifecycleRefused {
                        code: "frontmatter-invalid",
                        message,
                    }) = &e
                    {
                        out["error"]["message"] = json!(message);
                    }
                    if let Error::Store(StoreError::Stale { current, .. })
                    | Error::Lifecycle(LifecycleError::Store(StoreError::Stale {
                        current,
                        ..
                    })) = &e
                    {
                        out["error"]["current_checksum"] = json!(current);
                    }
                    if let Error::Store(StoreError::BudgetExhausted {
                        retry_at,
                        spent,
                        work,
                    })
                    | Error::Lifecycle(LifecycleError::Store(StoreError::BudgetExhausted {
                        retry_at,
                        spent,
                        work,
                    })) = &e
                    {
                        out["error"]["retry_at"] = json!(retry_at);
                        out["error"]["spent"] = json!(spent);
                        out["error"]["work"] = json!(work);
                    }
                    if let Error::Store(StoreError::Exists { current, .. })
                    | Error::Lifecycle(LifecycleError::Store(StoreError::Exists {
                        current,
                        ..
                    })) = &e
                    {
                        out["error"]["current_checksum"] = json!(current);
                    }
                    return result::bounded(out, true);
                }
            }
        }
        unreachable!("two-attempt read returns")
    }

    fn dispatch(&self, name: &str, a: &Value, out: &mut Value) -> Result<()> {
        match name {
            "wi_read" => {
                let read = self.resolve(required(a, "identifier")?)?;
                let mut hit = self.hit(&read, false)?;
                let sliced = a.get("section").is_some()
                    || a.get("start_line").is_some()
                    || a.get("end_line").is_some();
                if s(a, "section").is_some()
                    && (a.get("start_line").is_some() || a.get("end_line").is_some())
                {
                    return Err(Error::Invalid(
                        "section cannot be combined with start_line/end_line.",
                    ));
                }
                if !b(a, "include_content", true) && sliced {
                    return Err(Error::Invalid(
                        "include_content=false cannot be combined with section or lines.",
                    ));
                }
                if hit.lifecycle != "none"
                    && (sliced
                        || !b(a, "include_frontmatter", true)
                        || !b(a, "include_content", true))
                {
                    return Err(Error::Invalid(
                        "Lifecycle claims must be delivered whole with their qualifiers.",
                    ));
                }
                if !b(a, "include_frontmatter", true) {
                    hit.content = Note::parse(&read.bytes)
                        .map_err(StoreError::from)?
                        .body()
                        .to_owned();
                }
                if sliced {
                    if b(a, "include_frontmatter", true) {
                        hit.content = std::str::from_utf8(&read.bytes)
                            .map_err(|_| {
                                Error::Unsupported(
                                    "Binary files are unavailable through note tools.",
                                )
                            })?
                            .to_owned();
                    }
                    hit.content = slice(&hit.content, a)?;
                }
                if !b(a, "include_content", true) {
                    hit.content.clear();
                }
                out["index_as_of"] = json!(self.vault.index_as_of().ok().flatten());
                self.deliver(vec![hit], a, out)?;
            }
            "wi_write" => {
                crate::validate_frontmatter(required(a, "content")?.as_bytes())?;
                let r = self.lifecycle.write(
                    &self.vault,
                    WriteRequest {
                        path: required(a, "identifier")?.into(),
                        content: required(a, "content")?.as_bytes().to_vec(),
                        expected_checksum: owned(a, "expected_checksum"),
                        op_id: owned(a, "operation_id"),
                        ingress: Ingress::Mcp,
                    },
                )?;
                self.receipt(r, out)?;
            }
            "wi_edit" => {
                let command = required(a, "command")?;
                let section_command = matches!(
                    command,
                    "replace_section" | "insert_before_section" | "insert_after_section"
                );
                if section_command {
                    if let Some(first) = self.vault.replay_semantic_edit(s(a, "operation_id"), a)? {
                        return self.receipt(first, out);
                    }
                    let read = self.resolve(required(a, "identifier")?)?;
                    let note = Note::parse(&read.bytes).map_err(StoreError::from)?;
                    let body = note.body();
                    let range = crate::section::select_with(
                        body,
                        required(a, "section")?,
                        b(a, "replace_subsections", true),
                    )?;
                    let content = required(a, "content")?;
                    let first_line = body[range.start..]
                        .find('\n')
                        .map_or(range.end, |n| range.start + n + 1);
                    let header_end = if body[range.start..].starts_with('#') {
                        first_line
                    } else {
                        body[first_line..]
                            .find('\n')
                            .map_or(range.end, |n| first_line + n + 1)
                    };
                    let header_gap = if !content.is_empty() && !body[..header_end].ends_with('\n') {
                        "\n"
                    } else {
                        ""
                    };
                    let mut replacement = body.to_owned();
                    match command {
                        "replace_section" => {
                            let old = &body[header_end..range.end];
                            let lead = old.len() - old.trim_start_matches(['\r', '\n']).len();
                            let tail = old.len() - old.trim_end_matches(['\r', '\n']).len();
                            let prefix = &old[..lead];
                            let suffix = if lead + tail > old.len() {
                                ""
                            } else {
                                &old[old.len() - tail..]
                            };
                            replacement.replace_range(
                                header_end..range.end,
                                &format!(
                                    "{header_gap}{prefix}{}{suffix}",
                                    content.trim_matches(['\r', '\n'])
                                ),
                            );
                        }
                        "insert_before_section" => replacement.insert_str(
                            range.start,
                            &format!("{}\n\n", content.trim_end_matches(['\r', '\n'])),
                        ),
                        _ => replacement.insert_str(
                            header_end,
                            &format!("{header_gap}{}\n", content.trim_end_matches(['\r', '\n'])),
                        ),
                    }
                    if a.get("metadata").is_some()
                        || a.get("remove").is_some()
                        || a.get("find_text").is_some()
                    {
                        return Err(Error::Invalid(
                            "Section commands accept only section, content, and write preconditions.",
                        ));
                    }
                    let receipt = self.lifecycle.edit_semantic(
                        &self.vault,
                        EditRequest {
                            path: read.path,
                            set: vec![],
                            remove: vec![],
                            body: Some(BodyEdit::FindReplace {
                                find: body.to_owned(),
                                replace: replacement,
                                expected_replacements: 1,
                            }),
                            expected_checksum: owned(a, "expected_checksum")
                                .or(Some(read.checksum)),
                            op_id: owned(a, "operation_id"),
                            ingress: Ingress::Mcp,
                        },
                        a,
                    )?;
                    return self.receipt(receipt, out);
                }
                if a.get("section").is_some() || a.get("replace_subsections").is_some() {
                    return Err(Error::Invalid(
                        "section and replace_subsections require a section command.",
                    ));
                }
                if command != "find_replace" && a.get("find_text").is_some() {
                    return Err(Error::Invalid("find_text is only for find_replace."));
                }
                if command == "set_metadata" && a.get("content").is_some() {
                    return Err(Error::Invalid("set_metadata changes frontmatter only."));
                }
                if a["metadata"]
                    .as_object()
                    .is_some_and(|m| m.contains_key("title") || m.contains_key("permalink"))
                    || a["remove"]
                        .as_array()
                        .is_some_and(|m| m.iter().any(|k| k == "title" || k == "permalink"))
                {
                    return Err(Error::Invalid(
                        "Metadata edits cannot change title or permalink; use wi_move for file identity.",
                    ));
                }
                let id = required(a, "identifier")?;
                let path = match self.resolve(id) {
                    Ok(r) => r.path,
                    Err(Error::NotFound) | Err(Error::Store(StoreError::NotFound { .. }))
                        if a.get("operation_id").is_some() =>
                    {
                        id.to_owned()
                    }
                    Err(e) => return Err(e),
                };
                let command = required(a, "command")?;
                let body = match command {
                    "append" => Some(BodyEdit::Append {
                        content: required(a, "content")?.into(),
                    }),
                    "prepend" => Some(BodyEdit::Prepend {
                        content: required(a, "content")?.into(),
                    }),
                    "find_replace" => Some(BodyEdit::FindReplace {
                        find: required(a, "find_text")?.into(),
                        replace: required(a, "content")?.into(),
                        expected_replacements: n(a, "expected_replacements", 1),
                    }),
                    _ => None,
                };
                let set = a
                    .get("metadata")
                    .and_then(Value::as_object)
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                let remove = a
                    .get("remove")
                    .and_then(Value::as_array)
                    .map(|m| {
                        m.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                if command == "set_metadata"
                    && a.get("metadata").is_none()
                    && a.get("remove").is_none()
                {
                    return Err(Error::Invalid("set_metadata needs metadata or remove."));
                }
                if command != "set_metadata"
                    && (a.get("metadata").is_some() || a.get("remove").is_some())
                {
                    return Err(Error::Invalid("metadata and remove need set_metadata."));
                }
                let r = self.lifecycle.edit(
                    &self.vault,
                    EditRequest {
                        path,
                        set,
                        remove,
                        body,
                        expected_checksum: owned(a, "expected_checksum"),
                        op_id: owned(a, "operation_id"),
                        ingress: Ingress::Mcp,
                    },
                )?;
                self.receipt(r, out)?;
            }
            "wi_move" => self.move_files(a, out)?,
            "wi_archive" => {
                let r = self.lifecycle.archive(
                    &self.vault,
                    required(a, "identifier")?,
                    owned(a, "expected_checksum"),
                    owned(a, "operation_id"),
                )?;
                self.receipt(r, out)?;
            }
            "wi_search" => self.search(a, out)?,
            "wi_context" => {
                let identifier = required(a, "identifier")?;
                let root = self.resolve(identifier)?;
                let hit = self.hit(&root, false)?;
                let context = self.vault.context(&hit.permalink, n(a, "depth", 1))?;
                out["index_as_of"] = json!(context.index_as_of);
                out["diagnostics"] = json!(
                    context
                        .diagnostics
                        .iter()
                        .map(|d| d.message.as_str())
                        .collect::<Vec<_>>()
                );
                if context.truncated
                    && let Some(d) = out["diagnostics"].as_array_mut()
                {
                    d.push(json!("graph-truncated"));
                }
                let since = s(a, "since").map(since).transpose()?;
                let mut hits: Vec<_> = context
                    .hits
                    .into_iter()
                    .filter(|h| {
                        h.rel_path.to_string_lossy() == root.path
                            || since.is_none_or(|time| {
                                modified(self.vault.root(), &h.rel_path.to_string_lossy())
                                    .is_some_and(|m| m >= time)
                            })
                    })
                    .collect();
                hits.sort_by_key(|h| h.rel_path.to_string_lossy() != root.path);
                let cap = 1 + n(a, "max_related", 7);
                if hits.len() > cap {
                    result::withhold(out, "hit-limit", hits.len() - cap);
                }
                self.hydrate(hits.into_iter().take(cap).collect(), a, out)?;
                // Relation text can itself be memory. Never deliver an edge to
                // a withheld node, including an unresolved target string.
                let paths: Vec<_> = out["hits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|h| h["path"].as_str())
                    .collect();
                out["edges"] = json!(
                    context
                        .edges
                        .into_iter()
                        .filter(|e| paths.contains(&e.from_path.to_string_lossy().as_ref())
                            && e.to_path
                                .as_ref()
                                .is_some_and(|p| paths.contains(&p.to_string_lossy().as_ref())))
                        .collect::<Vec<_>>()
                );
            }
            "wi_list" | "wi_find" => self.list(name, a, out)?,
            "wi_history" => {
                let path = required(a, "identifier")?;
                match self.vault.read(path) {
                    Ok(_) | Err(StoreError::NotFound { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
                self.vault.flush()?;
                let rows = self
                    .vault
                    .history(path)?
                    .into_iter()
                    .map(|h| {
                        json!({"commit":h.commit,"blob":h.blob,"author_time":h.author_time,
                        "actor":h.trailers.iter().find(|(k,_)| k=="Wi-Actor").map(|(_,v)| v)})
                    })
                    .collect();
                out["items"] = json!(page(rows, a, out)?);
            }
            "wi_revert" => {
                let receipt = self.lifecycle.revert(
                    &self.vault,
                    required(a, "identifier")?,
                    required(a, "commit")?,
                    owned(a, "expected_checksum"),
                    owned(a, "operation_id"),
                )?;
                self.receipt(receipt, out)?;
            }
            "wi_capture" => out["data"] = self.lifecycle.capture(&self.vault, a)?,
            "wi_adopt" => out["data"] = self.lifecycle.adopt(&self.vault, a)?,
            "wi_lint" => out["data"] = self.lifecycle.lint(&self.vault, a)?,
            "wi_work" => out["data"] = self.lifecycle.work(&self.vault, a)?,
            _ => return Err(Error::Invalid("Unknown tool.")),
        }
        Ok(())
    }
}

fn slice(content: &str, a: &Value) -> Result<String> {
    let lines = content.split_inclusive('\n').collect::<Vec<_>>();
    if let Some(section) = s(a, "section") {
        let range = crate::section::select(content, section)?;
        return Ok(content[range].to_owned());
    }
    let start = n(a, "start_line", 1);
    let end = n(a, "end_line", lines.len());
    if start == 0 || end < start || start > lines.len() {
        return Err(Error::Invalid("Invalid line range."));
    }
    Ok(lines[start - 1..end.min(lines.len())].concat())
}

fn folder(path: &str) -> Result<&str> {
    if path.contains('\\')
        || path.chars().any(char::is_control)
        || (path != "/"
            && Path::new(path)
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)))
    {
        return Err(Error::Invalid("path must be a vault-relative folder."));
    }
    Ok(if path == "." || path == "/" {
        ""
    } else {
        path.trim_start_matches("./").trim_matches('/')
    })
}

fn generation_unavailable() -> CallToolResult {
    let mut out = result::envelope(0, None, true);
    out["generation"] = Value::Null;
    out["index"] = Value::Null;
    out["error"] = json!({"code":"generation-unavailable","message":"Cannot verify the vault generation; no memory was delivered."});
    result::bounded(out, true)
}

fn pagination(a: &Value) -> Result<(usize, usize)> {
    if a.get("offset").is_some() && a.get("page").is_some() {
        return Err(Error::Invalid("offset and page cannot be combined."));
    }
    let limit = n(a, "limit", 8);
    Ok((
        n(a, "offset", (n(a, "page", 1) - 1).saturating_mul(limit)),
        limit,
    ))
}
fn page_meta(total: usize, offset: usize, limit: usize, out: &mut Value) {
    out["offset"] = json!(offset);
    out["total"] = json!(total);
    let end = offset.saturating_add(limit).min(total);
    out["next_offset"] = if end < total { json!(end) } else { Value::Null };
    result::withhold(
        out,
        "hit-limit",
        total.saturating_sub(offset).saturating_sub(limit),
    );
}
fn page(rows: Vec<Value>, a: &Value, out: &mut Value) -> Result<Vec<Value>> {
    let (offset, limit) = pagination(a)?;
    page_meta(rows.len(), offset, limit, out);
    Ok(rows.into_iter().skip(offset).take(limit).collect())
}
fn modified(root: &Path, path: &str) -> Option<std::time::SystemTime> {
    std::fs::symlink_metadata(root.join(path))
        .ok()?
        .modified()
        .ok()
}
fn since(text: &str) -> Result<std::time::SystemTime> {
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(time.into());
    }
    if let Ok(day) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        && let Some(time) = day.and_hms_opt(0, 0, 0)
    {
        return Ok(time.and_utc().into());
    }
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (amount, unit) = text.split_at(split);
    let seconds = amount.parse::<u64>().ok().and_then(|n| {
        n.checked_mul(match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86400,
            "w" => 604800,
            _ => return None,
        })
    });
    seconds
        .and_then(|seconds| {
            std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(seconds))
        })
        .ok_or(Error::Invalid(
            "since must be RFC3339, YYYY-MM-DD or a relative interval such as 7d.",
        ))
}
fn message(code: &str) -> &'static str {
    match code {
        "not-found" => "The note, revision or section was not found.",
        "protected-path" => "This path is protected or crosses an unsafe filesystem component.",
        "locked" => "The note is locked. Unlock it in an editor before changing it.",
        "recovery-held" => {
            "This path has a recovery intent held because a file could not be read or renamed. Fix file permissions or the obstruction; the next operation retries automatically. Other paths are unaffected."
        }
        "conflict-markers" => "Resolve the note's merge conflict markers before editing it.",
        "engine-owned-field" => {
            "This detail is managed automatically; leave it out of the request."
        }
        "operation-mismatch" => {
            "This operation_id belongs to a different request. Use a new identifier."
        }
        "terminal-note" => "This note is kept as history. Save the corrected claim as a new note.",
        "lifecycle-unavailable" => {
            "Memory upkeep is unavailable in this connection. Reconnect to wiki-island."
        }
        "lifecycle-revert-requires-fork" => {
            "Copy the earlier claim into a new note so its history is kept."
        }
        "io" => "A filesystem operation failed. Check path length and local file permissions.",
        "too-large" => {
            "The file is larger than the read limit. Open it in its own app, or raise max_note_bytes in the settings file."
        }
        "parse" => "The note is not valid UTF-8.",
        "history" => "The requested history operation failed. Check the commit and vault history.",
        "index" => "Search needs to recover its local cache. Retry the request.",
        "config" => "The vault configuration could not be read or validated.",
        "watch" => "The filesystem watcher is unavailable.",
        "not-initialised" => "Open an initialized vault, or run wi init first.",
        "edit-refused" => {
            "The edit cannot preserve the note's format or satisfy its replacement preconditions."
        }
        "claim-edit-refused" => "Save the revised claim as a separate note for review.",
        "claim-edit-mixed" => {
            "A claim revision must contain claim fields only; send other edits separately."
        }
        "revert-refused" => "The earlier claim could not be restored as a new note.",
        "rule-retirement" => "Only a user can retire this rule.",
        "foreign-device-work" => "This work item belongs to another device and is read-only here.",
        "work-close-refused" => {
            "The work item needs an outcome, limits and a supported reopen trigger."
        }
        "capture-dropped" => "The capture contains only secrets or one-off values and was dropped.",
        "capture-empty" => "No usable capture content remains after filtering.",
        _ => "The request could not be completed; check the operation and try again.",
    }
}

fn markdown(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}
