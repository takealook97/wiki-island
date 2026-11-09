//! Commands that run inside an open vault session.

use std::io::Read;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use wi_store::{BodyEdit, EditRequest, Ingress, MoveRequest, Query, SearchMode, WriteRequest};

use super::Output;
use super::args::*;
use super::doctor::doctor;
use super::errors::document_message;
use super::session::{Session, flushed, ingest_warnings};
use super::standalone::sync_exports;
use super::{
    adoption_view, brief, import_paths, lifecycle_cmd, publish_cmd, service_cmd, sync_cmd,
};
use super::{error_codes, setup_cmd};

/// Longest search snippet shown under a hit.
pub(crate) const SNIPPET_CHARS: usize = 120;

pub(crate) fn run_in_vault(vault: &Session, command: Command) -> Result<Output> {
    match command {
        Command::Sync { command } => sync_cmd::run(vault, command),
        Command::Publish { command } => publish_cmd::run(vault, command),
        Command::Export {
            command: ExportCommand::Rules { host },
        } => {
            let hosts = match host.unwrap_or(RulesHost::All) {
                RulesHost::All => wi_export::Host::ALL.to_vec(),
                RulesHost::Claude => vec![wi_export::Host::Claude],
                RulesHost::Codex => vec![wi_export::Host::Codex],
                RulesHost::Gemini => vec![wi_export::Host::Gemini],
            };
            let reports = wi_export::export_rules(vault, &hosts)?;
            vault.flush()?;
            Ok(Output {
                human: reports
                    .iter()
                    .map(|r| {
                        format!(
                            "{}: {} bytes; {} included; {} omitted",
                            r.path,
                            r.bytes,
                            r.included.len(),
                            r.omitted.len()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                json: json!({"exports":reports}),
            })
        }
        Command::Service { command } => service_cmd::run(vault, command),
        Command::Skills { command } => {
            let home = setup_cmd::home()?;
            let targets = wi_export::skills::configured(vault);
            let apply = matches!(command, SkillsCommand::Sync);
            let report = wi_export::skills::sync(vault, &home, &targets, apply)?;
            Ok(Output {
                human: format!(
                    "Skills: {} new, {} updated, {} removed, {} up to date, {} kept as-is, {} use alternative names to preserve your skills{}.",
                    report.created,
                    report.updated,
                    report.removed,
                    report.unchanged,
                    report.skipped.len(),
                    report.renamed.len(),
                    if apply { "" } else { " (preview)" }
                ),
                json: serde_json::to_value(report)?,
            })
        }
        Command::Import { paths, ocr } => {
            let expanded = import_paths::expand(vault.root(), &paths);
            let paths = expanded.paths;
            let ocr = match ocr {
                OcrMode::Auto => wi_docs::Ocr::Auto,
                OcrMode::Off => wi_docs::Ocr::Off,
                OcrMode::Required => wi_docs::Ocr::Required,
            };
            let results =
                wi_docs::import_many(vault, &paths, wi_docs::ImportOptions { ocr, bulk: true })?;
            let mut reports: Vec<Value> = results
                .into_iter()
                .zip(&paths)
                .map(|(r, path)| match r {
                    Ok(report) => {
                        let mut value = serde_json::to_value(&report).unwrap_or_default();
                        if report.status == wi_docs::Status::Failed {
                            value["code"] = json!("document-extraction");
                        }
                        value
                    },
                    Err(e) => json!({"path":path,"status":"failed","code":error_codes::document(&e),"error":document_message(&e)}),
                })
                .collect();
            reports.extend(expanded.failures);
            let flush = vault.flush()?;
            vault.ingest_after_bulk_commit();
            let human = reports
                .iter()
                .map(|r| {
                    format!(
                        "{}: {}{}{}",
                        r.get("sidecar_path")
                            .or_else(|| r.get("path"))
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        r["status"].as_str().unwrap_or("failed"),
                        if r["unchanged"] == true {
                            " (unchanged)"
                        } else {
                            ""
                        },
                        r["error"]
                            .as_str()
                            .map(|e| format!(": {e}"))
                            .unwrap_or_else(|| {
                                if r["status"] == "failed" {
                                    r["diagnostics"]
                                        .as_array()
                                        .map(|d| {
                                            format!(
                                                ": {}",
                                                d.iter()
                                                    .filter_map(Value::as_str)
                                                    .collect::<Vec<_>>()
                                                    .join("; ")
                                            )
                                        })
                                        .unwrap_or_default()
                                } else {
                                    String::new()
                                }
                            })
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Output {
                json: json!({"imports":reports,"commits":flush.commits.len(),"remaining":flush.remaining,"blocked":flush.blocked.map(|b|format!("{b:?}"))}),
                human,
            })
        }
        Command::Write {
            path,
            stdin,
            file,
            content,
            expected_checksum,
            op_id,
        } => {
            let content = match (stdin, file, content) {
                (true, _, _) => {
                    let mut buf = Vec::new();
                    std::io::stdin().read_to_end(&mut buf)?;
                    buf
                }
                (_, Some(file), _) => {
                    std::fs::read(&file).with_context(|| format!("read {}", file.display()))?
                }
                (_, _, Some(content)) => content.into_bytes(),
                _ => anyhow::bail!("give the content with --stdin, --file or --content"),
            };
            wi_mcp::validate_frontmatter(&content)?;
            let result = vault.lifecycle_write(WriteRequest {
                path,
                content,
                expected_checksum,
                op_id,
                ingress: Ingress::Cli,
            })?;
            write_output(vault, result, "wrote")
        }
        Command::Edit {
            path,
            set,
            remove,
            append,
            prepend,
            find,
            replace,
            count,
            expected_checksum,
            op_id,
        } => {
            let body = match (append, prepend, find, replace) {
                (Some(content), _, _, _) => Some(BodyEdit::Append { content }),
                (_, Some(content), _, _) => Some(BodyEdit::Prepend { content }),
                (_, _, Some(find), Some(replace)) => Some(BodyEdit::FindReplace {
                    find,
                    replace,
                    expected_replacements: count,
                }),
                _ => None,
            };
            let set = kv(&set)?;
            if set.is_empty() && remove.is_empty() && body.is_none() {
                anyhow::bail!(
                    "nothing to edit: give --set, --remove, --append, --prepend or --find/--replace"
                );
            }
            let result = vault.lifecycle_edit(EditRequest {
                path,
                set,
                remove,
                body,
                expected_checksum,
                op_id,
                ingress: Ingress::Cli,
            })?;
            write_output(vault, result, "edited")
        }
        Command::Move {
            from,
            to,
            expected_checksum,
            op_id,
        } => {
            let result = vault.lifecycle_move(MoveRequest {
                from,
                to,
                expected_checksum,
                op_id,
                ingress: Ingress::Cli,
            })?;
            write_output(vault, result, "moved to")
        }
        Command::Archive {
            path,
            expected_checksum,
            op_id,
        } => {
            let result = vault.lifecycle_archive(&path, expected_checksum, op_id, Ingress::Cli)?;
            write_output(vault, result, "archived to")
        }
        Command::Read { path } => {
            let read = vault.read(&path)?;
            let text = String::from_utf8_lossy(&read.bytes).into_owned();
            let mut json = serde_json::to_value(&read)?;
            json["content"] = Value::String(text.clone());
            Ok(Output { json, human: text })
        }
        Command::Search {
            query,
            mode,
            filter,
            note_type,
            limit,
            read_mode,
            condition,
        } => {
            let conditions: serde_json::Map<String, Value> = kv(&condition)?.into_iter().collect();
            let metadata_filters = kv(&filter)?.into_iter().collect();
            let query = Query {
                text: query,
                mode: match mode {
                    Mode::Text => SearchMode::Text,
                    Mode::Title => SearchMode::Title,
                    Mode::Permalink => SearchMode::Permalink,
                },
                metadata_filters,
                type_filter: note_type,
                limit,
                count_limit: 0,
            };
            let (response, labels) =
                lifecycle_cmd::search(vault, &query, &Value::Object(conditions), read_mode)?;
            let mut human = String::new();
            for hit in &response.hits {
                human.push_str(&format!(
                    "{}  {}{}\n",
                    hit.rel_path.display(),
                    hit.title,
                    if hit.fallback { "  [fallback]" } else { "" }
                ));
                let snippet = one_line_snippet(&hit.snippet);
                if !snippet.is_empty() {
                    human.push_str(&format!("    {snippet}\n"));
                }
            }
            if response.hits.is_empty() {
                human.push_str("no matches\n");
            }
            human.push_str(&format!(
                "index as of {} · generation {}",
                response
                    .index_as_of
                    .as_deref()
                    .unwrap_or("never (literal scan)"),
                response.generation
            ));
            for withheld in &response.withheld {
                human.push_str(&format!(
                    "\nwithheld: {} ({})",
                    withheld.count, withheld.reason
                ));
            }
            if response.withheld.iter().any(|w| {
                matches!(
                    w.reason.as_str(),
                    "stale"
                        | "drift"
                        | "conflict"
                        | "condition-unknown"
                        | "not-current"
                        | "expired"
                )
            }) {
                human.push_str("\nhint: --read-mode all includes notes that need review");
            }
            let mut json = serde_json::to_value(&response)?;
            json["lifecycle"] = serde_json::to_value(&labels)?;
            Ok(Output { json, human })
        }
        Command::Capture(args) => lifecycle_cmd::capture(vault, args),
        Command::Adopt { path } => lifecycle_cmd::adopt(vault, &path),
        Command::Lint { path } => lifecycle_cmd::lint(vault, path.as_deref()),
        Command::Sweep => lifecycle_cmd::sweep(vault),
        Command::Work { command } => lifecycle_cmd::work(vault, command),
        Command::Brief => brief::brief(vault),
        Command::Maintain { full } => {
            let report = vault.maintain(&wi_store::MaintainOptions {
                full,
                time_budget: None,
            })?;
            let tasks = vault
                .work_list()?
                .iter()
                .filter(|w| w.actionable && w.state.as_deref() == Some("ready"))
                .count();
            let mut human = if report.procedures_changed
                || report.repaired > 0
                || report.index_rebuilt
                || report.work_created > 0
                || report.work_closed > 0
                || report.migrated > 0
            {
                format!(
                    "Maintenance done: {} repaired, {} to-do item(s) added, {} closed, {} moved{}{}.",
                    report.repaired,
                    report.work_created,
                    report.work_closed,
                    report.migrated,
                    if report.index_rebuilt {
                        ", search index rebuilt"
                    } else {
                        ""
                    },
                    if report.procedures_changed {
                        ", procedures changed"
                    } else {
                        ""
                    }
                )
            } else {
                "Everything is in order.".into()
            };
            if tasks > 0 {
                human = if human == "Everything is in order." {
                    format!("{tasks} task(s) for your AI.")
                } else {
                    format!("{human} {tasks} task(s) for your AI.")
                };
            }
            sync_exports(vault);
            let mut json = serde_json::to_value(&report)?;
            json["tasks_ready"] = json!(tasks);
            Ok(Output { human, json })
        }
        Command::Reindex => {
            let (report, missing) = vault.reindex_with_missing()?;
            let mut human = format!(
                "reindexed {} note(s) in {} ms",
                report.discovered, report.elapsed_ms
            );
            for path in &missing {
                human.push_str(&format!("\nmissing: {path} (recoverable from history)"));
            }
            let mut json = serde_json::to_value(&report)?;
            json["missing"] = serde_json::to_value(&missing)?;
            Ok(Output { json, human })
        }
        Command::Commit => {
            let reconcile = vault.reconcile()?;
            vault
                .warnings
                .borrow_mut()
                .extend(ingest_warnings(&reconcile));
            let flush = vault.flush()?;
            let mut human = format!(
                "ingested {} changed file(s), {} commit(s)",
                reconcile.ingested.len(),
                flush.commits.len()
            );
            if let Some(blocked) = &flush.blocked {
                human.push_str(&format!("\nflush blocked: {blocked:?}; run `wi doctor`"));
            }
            for missing in &reconcile.missing {
                human.push_str(&format!("\nmissing: {missing} (recoverable from history)"));
            }
            Ok(Output {
                json: json!({"reconcile": reconcile, "flush": {
                    "commits": flush.commits.len(),
                    "remaining": flush.remaining,
                    "blocked": flush.blocked.map(|b| format!("{b:?}")),
                }}),
                human,
            })
        }
        Command::History { path } => {
            vault.flush()?;
            let entries = vault.history(&path)?;
            let human = entries
                .iter()
                .map(|e| format!("{}  {}", &e.commit[..12.min(e.commit.len())], e.summary))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Output {
                json: json!(entries),
                human: if human.is_empty() {
                    "no history".into()
                } else {
                    human
                },
            })
        }
        Command::Revert { path, commit } => {
            let result = vault.lifecycle_revert(&path, &commit, None, None, Ingress::Cli)?;
            write_output(vault, result, "reverted")
        }
        Command::Doctor => doctor(vault),
        Command::Setup { .. }
        | Command::Uninstall
        | Command::Mcp { .. }
        | Command::Note { .. }
        | Command::Init { .. }
        | Command::Watch { .. } => unreachable!("standalone commands are dispatched by run"),
    }
}

pub(crate) fn kv(items: &[String]) -> Result<Vec<(String, Value)>> {
    items
        .iter()
        .map(|item| {
            let (key, value) = item
                .split_once('=')
                .with_context(|| format!("{item:?} is not KEY=VALUE"))?;
            let value = serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.into()));
            Ok((key.to_owned(), value))
        })
        .collect()
}

pub(crate) fn write_output(
    vault: &Session,
    result: wi_store::WriteResult,
    verb: &str,
) -> Result<Output> {
    let mut human = format!(
        "{verb} {} ({}, seq {}{})",
        result.path,
        result.checksum,
        result.seq,
        if result.replayed { ", replayed" } else { "" }
    );
    if let Some(copy) = &result.conflict_copy {
        human.push_str(&format!("\nconflict: an editor save was kept at {copy}"));
    }
    if result.index == wi_store::IndexState::Pending {
        human.push_str("\nindex: pending (repaired on the next run)");
    }
    if let Some(explained) = adoption_view::explain(result.lifecycle.as_ref()) {
        human.push('\n');
        human.push_str(&explained);
    }
    let json = flushed(vault, serde_json::to_value(&result)?)?;
    Ok(Output { json, human })
}

pub(crate) fn one_line_snippet(raw: &str) -> String {
    let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= SNIPPET_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(SNIPPET_CHARS).collect();
    cut.push('…');
    cut
}
