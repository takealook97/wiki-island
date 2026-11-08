//! Lifecycle commands: `wi capture`, `wi adopt`, `wi lint`, `wi sweep`,
//! `wi work`, and the read modes of `wi search`.

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};
use wi_store::{
    CaptureInput, Classified, Ingress, Proposal, Query, SearchOptions, SearchResponse,
    SnapshotInput, Vault, WorkClose,
};

use super::{Output, Session, flushed};

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ReadModeArg {
    Current,
    #[value(alias = "historic", alias = "explore")]
    All,
}

#[derive(Args)]
#[command(group = clap::ArgGroup::new("input").required(true).multiple(false))]
pub(crate) struct CaptureArgs {
    /// The captured text (or use --stdin or --file).
    #[arg(group = "input", allow_hyphen_values = true)]
    text: Option<String>,
    /// Read the captured text from standard input.
    #[arg(long, group = "input")]
    stdin: bool,
    /// Read the captured text from a UTF-8 file.
    #[arg(long, group = "input", value_name = "PATH")]
    file: Option<std::path::PathBuf>,
    /// Title for the capture note.
    #[arg(long)]
    title: Option<String>,
    /// Why an empty capture yields no memory.
    #[arg(long)]
    no_memory_reason: Option<String>,
    /// URL of a host-fetched page; needs --snapshot-path and --snapshot-file.
    #[arg(long, requires_all = ["snapshot_path", "snapshot_file"])]
    url: Option<String>,
    /// Vault path of the snapshot file (under the captures directory).
    #[arg(long)]
    snapshot_path: Option<String>,
    /// Local file holding the fetched bytes.
    #[arg(long)]
    snapshot_file: Option<std::path::PathBuf>,
    /// Proposed candidate: vault path=local file with its Markdown.
    #[arg(long = "candidate", value_name = "PATH=FILE")]
    candidates: Vec<String>,
}

#[derive(Subcommand)]
pub(crate) enum WorkCommand {
    /// List work items.
    List,
    /// Close a work item with an outcome.
    Close {
        /// Vault path of the work item.
        path: String,
        /// adopted, narrowed, alternative, not_supported,
        /// evidence_unavailable or closed_without_supported_answer.
        #[arg(long)]
        outcome: String,
        /// Limits of the answer, recorded with the outcome.
        #[arg(long)]
        limits: Option<String>,
        /// session, source-change, query or cooldown.
        #[arg(long)]
        reopen_trigger: Option<String>,
        /// Detail of the reopen trigger (for example the query or cooldown).
        #[arg(long)]
        reopen_detail: Option<String>,
    },
    /// Open a conflict work item for two claims without a shared slot.
    OpenConflict {
        /// Vault path of the first claim.
        a: String,
        /// Vault path of the second claim.
        b: String,
        /// Why the claims conflict.
        #[arg(long, default_value = "claims conflict without a slot")]
        reason: String,
    },
}

/// Search in a read mode. `current` withholds ineligible lifecycle notes;
/// the other modes label every hit.
pub(crate) fn search(
    vault: &Vault,
    query: &Query,
    conditions: &Value,
    mode: ReadModeArg,
) -> Result<(SearchResponse, Vec<Classified>)> {
    let options = SearchOptions { catch_up: true };
    if mode == ReadModeArg::Current {
        let found = vault.search_current(query, conditions, options)?;
        return Ok((found.response, found.labels));
    }
    let response = vault.search(query, options)?;
    let read: Vec<(String, Vec<u8>)> = response
        .hits
        .iter()
        .filter_map(|h| {
            let path = h.rel_path.to_string_lossy().into_owned();
            vault.read(&path).ok().map(|r| (path, r.bytes))
        })
        .collect();
    let labels = vault.classify(&read, conditions)?;
    Ok((response, labels))
}

fn capture_text(args: &CaptureArgs) -> Result<String> {
    use std::io::Read;
    let bytes = if args.stdin {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("read standard input")?;
        buf
    } else if let Some(file) = &args.file {
        std::fs::read(file).with_context(|| format!("read {}", file.display()))?
    } else if let Some(text) = &args.text {
        return Ok(text.clone());
    } else {
        anyhow::bail!("give the text as an argument, --stdin or --file");
    };
    String::from_utf8(bytes).context("the capture text is not valid UTF-8")
}

pub(crate) fn capture(vault: &Session, args: CaptureArgs) -> Result<Output> {
    let text = capture_text(&args)?;
    let snapshot = match (args.url, args.snapshot_path, args.snapshot_file) {
        (Some(url), Some(path), Some(file)) => Some(SnapshotInput {
            url,
            path,
            content: std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?,
        }),
        _ => None,
    };
    let proposals = args
        .candidates
        .iter()
        .map(|item| {
            let (path, file) = item
                .split_once('=')
                .with_context(|| format!("{item:?} is not PATH=FILE"))?;
            Ok(Proposal {
                path: path.to_owned(),
                content: std::fs::read_to_string(file).with_context(|| format!("read {file}"))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let input = CaptureInput {
        text,
        title: args.title,
        no_memory_reason: args.no_memory_reason,
        snapshot,
        proposals,
    };
    let value = vault.capture(&input, Ingress::Cli)?;
    let human = format!(
        "captured {} ({})",
        value["capture"].as_str().unwrap_or_default(),
        value["capture_status"].as_str().unwrap_or_default()
    );
    Ok(Output {
        json: flushed(vault, value)?,
        human,
    })
}

pub(crate) fn adopt(vault: &Session, path: &str) -> Result<Output> {
    // Owner adoption first: the agent label goes, then the gates run.
    let owned = vault.take_ownership(path)?;
    let report = vault.adopt(path)?;
    let resumed = vault.resume_deferred()?;
    let mut human = format!("{}: {}", report.path, report.outcome);
    if owned.is_some() {
        human.insert_str(0, "adopted as yours (agent label removed)\n");
    }
    let adoption = serde_json::to_value(&report)?;
    if let Some(explained) = super::adoption_view::explain(Some(&json!({"adoption": adoption}))) {
        human.push('\n');
        human.push_str(&explained);
    }
    Ok(Output {
        json: flushed(
            vault,
            json!({"adoption": report, "resumed": resumed, "owned": owned.is_some()}),
        )?,
        human,
    })
}

pub(crate) fn lint(vault: &Session, path: Option<&str>) -> Result<Output> {
    let reports = vault.lint(path)?;
    let human = if reports.iter().all(|r| r.findings.is_empty()) {
        "lint clean".to_owned()
    } else {
        reports
            .iter()
            .flat_map(|r| {
                r.findings
                    .iter()
                    .map(move |f| format!("{}: {:?} {}", r.path, f.code, f.message))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(Output {
        json: json!({"reports": reports}),
        human,
    })
}

pub(crate) fn sweep(vault: &Session) -> Result<Output> {
    let summary = vault.sweep()?;
    let human = format!(
        "sweep: {} resumed, {} recovered, {} conflict flag(s), {} work item(s) created, {} refreshed, {} request(s) converted",
        summary.resumed.len(),
        summary.recovered.len(),
        summary.conflicts_flagged.len(),
        summary.work_created.len(),
        summary.work_refreshed.len(),
        summary.requests_converted.len()
    );
    Ok(Output {
        json: flushed(vault, serde_json::to_value(&summary)?)?,
        human,
    })
}

pub(crate) fn work(vault: &Session, command: WorkCommand) -> Result<Output> {
    match command {
        WorkCommand::List => {
            let items = vault.work_list()?;
            let human = items
                .iter()
                .map(|i| {
                    format!(
                        "{}  {}  {}",
                        i.path,
                        i.state.as_deref().unwrap_or("?"),
                        i.reason.as_deref().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Output {
                json: json!({"items": items}),
                human: if human.is_empty() {
                    "no work items".into()
                } else {
                    human
                },
            })
        }
        WorkCommand::Close {
            path,
            outcome,
            limits,
            reopen_trigger,
            reopen_detail,
        } => {
            let request: WorkClose = serde_json::from_value(json!({
                "path": path,
                "outcome": outcome,
                "limits": limits,
                "reopen_trigger": reopen_trigger,
                "reopen_detail": reopen_detail,
            }))
            .context("outcome or reopen trigger is not a known value")?;
            let closed = vault.work_close(&request)?;
            Ok(Output {
                json: flushed(vault, json!({"closed": closed}))?,
                human: format!("closed {closed}"),
            })
        }
        WorkCommand::OpenConflict { a, b, reason } => {
            let opened = vault.work_open_conflict(&a, &b, &reason)?;
            Ok(Output {
                json: flushed(vault, json!({"opened": opened}))?,
                human: format!("opened {opened}"),
            })
        }
    }
}
