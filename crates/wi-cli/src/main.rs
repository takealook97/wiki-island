#![forbid(unsafe_code)]

#[path = "adoption_view.rs"]
mod adoption_view;
mod brief;
mod error_codes;
mod import_paths;
#[path = "lifecycle_cmd.rs"]
mod lifecycle_cmd;
#[path = "service_cmd.rs"]
mod service_cmd;
#[path = "setup_cmd.rs"]
mod setup_cmd;
mod sync_cmd;

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use wi_note::Note;
use wi_store::{
    BodyEdit, EditRequest, Ingress, MoveRequest, Query, SearchMode, StoreError, Vault, WatchEvent,
    WriteRequest,
};

/// Longest search snippet shown under a hit.
const SNIPPET_CHARS: usize = 120;

#[derive(Parser)]
#[command(name = "wi", version, about = "Wiki Island personal knowledge engine")]
struct Cli {
    /// Notes folder (default: $WI_VAULT, else ~/WikiIsland; uses the current folder when it is a vault).
    #[arg(long, global = true, env = "WI_VAULT")]
    vault: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum InstallHost {
    Claude,
    Codex,
    /// Antigravity IDE.
    Agy,
    /// Gemini CLI.
    Gemini,
    Cursor,
    Windsurf,
    ClaudeDesktop,
    Vscode,
    Opencode,
    Zed,
}
impl From<InstallHost> for wi_export::hosts::HostId {
    fn from(host: InstallHost) -> Self {
        match host {
            InstallHost::Claude => Self::Claude,
            InstallHost::Codex => Self::Codex,
            InstallHost::Agy => Self::Agy,
            InstallHost::Gemini => Self::Gemini,
            InstallHost::Cursor => Self::Cursor,
            InstallHost::Windsurf => Self::Windsurf,
            InstallHost::ClaudeDesktop => Self::ClaudeDesktop,
            InstallHost::Vscode => Self::Vscode,
            InstallHost::Opencode => Self::Opencode,
            InstallHost::Zed => Self::Zed,
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum RulesHost {
    Claude,
    Codex,
    Gemini,
    All,
}
#[derive(Subcommand)]
enum McpCommand {
    /// Preview host registration; apply with --yes and back up the original.
    Install {
        /// Installed AI app to connect.
        #[arg(long, value_enum)]
        host: InstallHost,
        /// Apply the change; without it only a preview is shown.
        #[arg(long)]
        yes: bool,
    },
}
#[derive(Subcommand)]
enum ServiceCommand {
    /// Install a LaunchAgent that keeps this vault ingested, committed and indexed.
    Install {
        /// Apply the change; without it only a preview is shown.
        #[arg(long)]
        yes: bool,
        /// Allow a development build path (unstable across builds).
        #[arg(long)]
        allow_dev_binary: bool,
    },
    /// Unload the LaunchAgent and remove its plist.
    Uninstall,
    /// Show whether the LaunchAgent is installed and loaded.
    Status,
}
#[derive(Subcommand)]
enum ExportCommand {
    Rules {
        /// Host; omit to generate all three fragments.
        #[arg(long, value_enum)]
        host: Option<RulesHost>,
    },
}

#[derive(Subcommand)]
enum SkillsCommand {
    /// Update the installed copies of your procedures.
    Sync,
    /// Preview which procedure copies need updating.
    Status,
}
#[derive(Subcommand)]
enum Command {
    /// Set up your notes and connect all installed AI apps.
    Setup {
        /// Make this device the one that runs lifecycle maintenance of a
        /// notes folder shared by a file-sync service.
        #[arg(long)]
        primary: bool,
        /// Connect a private remote repository using the authenticated GitHub CLI.
        #[arg(long)]
        github: bool,
    },
    /// Synchronize notes with your configured git remote.
    Sync {
        #[command(subcommand)]
        command: Option<sync_cmd::SyncCommand>,
    },
    /// Remove Wiki Island connections and background care; keep your notes.
    Uninstall,
    /// Keep AI app skills up to date with your procedures.
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    /// Serve local MCP tools over standard input/output.
    Mcp {
        #[command(subcommand)]
        command: Option<McpCommand>,
    },
    /// Run the vault in the background (macOS LaunchAgent).
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Generate a protected, read-only host fragment from current user rules.
    Export {
        #[command(subcommand)]
        command: ExportCommand,
    },
    /// Parse one note file and print its structure.
    #[command(hide = true)]
    Note {
        #[command(subcommand)]
        command: NoteCommand,
    },
    /// Create a vault: git repository, config, .gitignore, first commit.
    Init {
        /// Directory to create the vault in (default: --vault, else the current directory).
        path: Option<PathBuf>,
    },
    /// Import local documents or a Notion Markdown/CSV export.
    Import {
        /// Files or directories to import.
        #[arg(required = true)]
        paths: Vec<String>,
        /// OCR for images and scanned documents: auto, off or required.
        #[arg(long, value_enum, default_value_t = OcrMode::Auto)]
        ocr: OcrMode,
    },
    /// Create a note, or replace it with --expected-checksum.
    Write {
        /// Vault path of the note.
        path: String,
        /// Read the content from standard input.
        #[arg(long, conflicts_with_all = ["file", "content"])]
        stdin: bool,
        /// Read the content from a file.
        #[arg(long, conflicts_with = "content")]
        file: Option<PathBuf>,
        /// The content as an argument (may start with `---`).
        #[arg(long, allow_hyphen_values = true)]
        content: Option<String>,
        /// Checksum from `wi read --json`. Required to replace with `wi write`; optional elsewhere, and a stale value is refused.
        #[arg(long)]
        expected_checksum: Option<String>,
        /// Operation id; repeating it returns the earlier result instead of writing again.
        #[arg(long)]
        op_id: Option<String>,
    },
    /// Byte-preserving frontmatter edit and body edits.
    Edit {
        /// Vault path of the note.
        path: String,
        /// Set a frontmatter key: key=value (value read as JSON, else as text).
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Remove a frontmatter key.
        #[arg(long = "remove", value_name = "KEY")]
        remove: Vec<String>,
        /// Text to add at the end of the body.
        #[arg(long, conflicts_with_all = ["prepend", "find"], allow_hyphen_values = true)]
        append: Option<String>,
        /// Text to add at the start of the body.
        #[arg(long, conflicts_with = "find", allow_hyphen_values = true)]
        prepend: Option<String>,
        /// Body text to find (needs --replace).
        #[arg(long, requires = "replace", allow_hyphen_values = true)]
        find: Option<String>,
        /// Replacement for --find (needs --find).
        #[arg(long, requires = "find", allow_hyphen_values = true)]
        replace: Option<String>,
        /// Occurrences --find must have.
        #[arg(long, default_value_t = 1)]
        count: usize,
        /// Checksum from `wi read --json`. Required to replace with `wi write`; optional elsewhere, and a stale value is refused.
        #[arg(long)]
        expected_checksum: Option<String>,
        /// Operation id; repeating it returns the earlier result instead of writing again.
        #[arg(long)]
        op_id: Option<String>,
    },
    /// Move or rename a note; its permalink is kept.
    Move {
        /// Current vault path of the note.
        from: String,
        /// New vault path.
        to: String,
        /// Checksum from `wi read --json`. Required to replace with `wi write`; optional elsewhere, and a stale value is refused.
        #[arg(long)]
        expected_checksum: Option<String>,
        /// Operation id; repeating it returns the earlier result instead of writing again.
        #[arg(long)]
        op_id: Option<String>,
    },
    /// Move a note under the archive directory (there is no delete).
    Archive {
        /// Vault path of the note.
        path: String,
        /// Checksum from `wi read --json`. Required to replace with `wi write`; optional elsewhere, and a stale value is refused.
        #[arg(long)]
        expected_checksum: Option<String>,
        /// Operation id; repeating it returns the earlier result instead of writing again.
        #[arg(long)]
        op_id: Option<String>,
    },
    /// Print a note with its checksum.
    Read {
        /// Vault path of the note.
        path: String,
    },
    /// Search notes, including recent changes.
    Search {
        /// Text to search for.
        query: String,
        /// Where to match: text, title or permalink.
        #[arg(long, value_enum, default_value_t = Mode::Text)]
        mode: Mode,
        /// Metadata filter key=value (value read as JSON, else as text).
        #[arg(long = "filter", value_name = "KEY=VALUE")]
        filter: Vec<String>,
        /// Only notes of this type.
        #[arg(long = "type")]
        note_type: Option<String>,
        /// Most hits to return.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// `current` gives usable notes; `all` includes notes needing review.
        #[arg(long, value_enum, default_value_t = lifecycle_cmd::ReadModeArg::Current)]
        read_mode: lifecycle_cmd::ReadModeArg,
        /// Query condition key=value for applicability conditions.
        #[arg(long = "condition", value_name = "KEY=VALUE")]
        condition: Vec<String>,
    },
    /// Show the standing brief: pinned notes, then rules, then preferences.
    Brief,
    /// Tidy the vault once: repair, refresh to-do items and procedures.
    Maintain {
        /// Also compare every file with the index.
        #[arg(long)]
        full: bool,
    },
    /// Rebuild the device index from the files.
    #[command(hide = true)]
    Reindex,
    /// Ingest unseen file changes and commit pending records.
    #[command(hide = true)]
    Commit,
    /// Show a note's git history.
    History {
        /// Vault path of the note.
        path: String,
    },
    /// Restore a note's bytes from a commit, as a new commit.
    Revert {
        /// Vault path of the note.
        path: String,
        /// Commit (from `wi history`) whose bytes to restore.
        commit: String,
    },
    /// Watch the vault and ingest editor changes until interrupted.
    Watch {
        /// Stop after this many seconds (default: run until interrupted).
        #[arg(long)]
        duration: Option<u64>,
    },
    /// Check note history, search and background care.
    Doctor,
    /// Store a capture note (filtered); optionally a URL snapshot and candidates.
    Capture(lifecycle_cmd::CaptureArgs),
    /// Adopt a note as yours: remove its agent label (agent origin or
    /// `noted_by: agent`), then rerun the lifecycle gates and apply the
    /// outcome. Removing the label in any editor does the same.
    #[command(hide = true)]
    Adopt {
        /// Vault path of the note.
        path: String,
    },
    /// Model-free lint of one note or of every memory note.
    #[command(hide = true)]
    Lint {
        /// Vault path of one note (default: every memory note).
        path: Option<String>,
    },
    /// Resume deferred changes, complete supersedes, flag conflicts and
    /// create or refresh work items. Never changes a status otherwise.
    #[command(hide = true)]
    Sweep,
    /// List, close or open work items.
    #[command(hide = true)]
    Work {
        #[command(subcommand)]
        command: lifecycle_cmd::WorkCommand,
    },
}

#[derive(Subcommand)]
enum NoteCommand {
    Parse {
        /// Markdown file to parse.
        file: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Text,
    Title,
    Permalink,
}

#[derive(Clone, Copy, ValueEnum)]
enum OcrMode {
    Auto,
    Off,
    Required,
}

/// Parse the arguments. With `--json`, a usage error is printed as a JSON
/// error object (exit 2) instead of clap's text.
fn parse_args() -> Cli {
    let wants_json = std::env::args_os().any(|a| a == "--json");
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if wants_json && e.use_stderr() => {
            let body = json!({"error": {
                "code": "usage",
                "message": e.kind().to_string(),
                "detail": e.render().to_string(),
                "hint": "run `wi <command> --help`; give content with --content=VALUE, --file or --stdin",
            }});
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
            std::process::exit(2);
        }
        Err(e) => e.exit(),
    }
}

pub(crate) struct Output {
    pub(crate) json: Value,
    pub(crate) human: String,
}

pub(crate) fn main() -> Result<()> {
    let cli = parse_args();
    let json = cli.json && !matches!(cli.command, Command::Mcp { command: None });
    let ndjson = matches!(cli.command, Command::Watch { .. });
    match run(cli) {
        Ok(out) => {
            let failed_import = out
                .json
                .get("imports")
                .and_then(Value::as_array)
                .is_some_and(|reports| reports.iter().any(|r| r["status"] == "failed"));
            if json {
                println!(
                    "{}",
                    if ndjson {
                        serde_json::to_string(&out.json)?
                    } else {
                        serde_json::to_string_pretty(&out.json)?
                    }
                );
            } else if !out.human.is_empty() {
                println!("{}", out.human.trim_end());
            }
            if failed_import {
                std::process::exit(1);
            }
            Ok(())
        }
        Err(error) => {
            let (code, hint) = match error.downcast_ref::<StoreError>() {
                Some(e) => (e.code(), hint(e)),
                None => (error_codes::code(&error), None),
            };
            if json {
                let warnings = error
                    .downcast_ref::<IngestWarnings>()
                    .map(|w| w.0.as_slice())
                    .unwrap_or(&[]);
                let body = json!({"error": {"code": code, "message": plain_error(&error), "hint": hint}, "warnings": warnings});
                println!(
                    "{}",
                    if ndjson {
                        serde_json::to_string(&body)?
                    } else {
                        serde_json::to_string_pretty(&body)?
                    }
                );
            }
            eprintln!("error: {}", plain_error(&error));
            if let Some(hint) = hint {
                eprintln!("hint: {hint}");
            }
            std::process::exit(1);
        }
    }
}

fn plain_error(error: &anyhow::Error) -> String {
    match error.downcast_ref::<StoreError>() {
        Some(StoreError::EngineOwnedField { field, .. }) => {
            format!("The detail '{field}' is managed automatically.")
        }
        Some(StoreError::ClaimEditRequiresCandidate { path, .. }) => {
            format!("Save the revised claim in {path} as a new note so its history is kept.")
        }
        Some(
            StoreError::LifecycleRevertRequiresFork { path } | StoreError::TerminalNote { path },
        ) => format!("The claim in {path} is kept as history; restore its text as a new note."),
        Some(StoreError::Index(_)) => {
            "Search is recovering its local cache. Your notes are safe.".into()
        }
        Some(StoreError::LifecycleRefused { message, .. }) => message.clone(),
        _ => format!("{error:#}"),
    }
}

fn hint(error: &StoreError) -> Option<&'static str> {
    Some(match error {
        StoreError::NotInitialised(_) => "run `wi init` in the vault directory or pass --vault",
        StoreError::Exists { .. } => {
            "run `wi read <path> --json` and pass its checksum with --expected-checksum"
        }
        StoreError::Stale { .. } => "read the note again and retry with the new checksum",
        StoreError::Locked { .. } => "edit the file directly to remove `locked: true`",
        StoreError::ConflictMarkers { .. } => "resolve the git conflict markers in an editor",
        StoreError::Protected { .. } => "choose a normal note folder inside your vault",
        StoreError::EngineOwnedField { .. } => {
            "leave automatically managed details out of the request"
        }
        StoreError::ClaimEditRequiresCandidate { .. } => {
            "save the corrected claim as a new note and link it to the previous claim"
        }
        StoreError::LifecycleRevertRequiresFork { .. } => {
            "copy the earlier text into a new note so its history is kept"
        }
        StoreError::OperationMismatch { .. } => "use a new --op-id for a different request",
        StoreError::Index(_) => "search will recover automatically on the next run",
        StoreError::History(_) => "run `wi doctor`",
        _ => return None,
    })
}

/// Refresh the host skill copies of this vault's procedures. Problems are warnings: notes and
/// maintenance are unaffected, and the next pass tries again.
fn sync_exports(vault: &Vault) {
    if let Err(e) = wi_export::Regenerator::default().poll(vault, false) {
        eprintln!("warning: rules were not updated: {e}");
    }
    if let Err(e) = wi_export::skills::sync_recorded(vault) {
        eprintln!("warning: skills were not updated: {e}");
    }
}

fn vault_root(cli_vault: &Option<PathBuf>) -> PathBuf {
    cli_vault.clone().unwrap_or_else(|| {
        if !std::path::Path::new(".wiki-island/config.toml").is_file()
            && let Some(home) = std::env::var_os("HOME")
        {
            let default = PathBuf::from(home).join("WikiIsland");
            if default.join(".wiki-island/config.toml").is_file() {
                return default;
            }
        }
        PathBuf::from(".")
    })
}

fn open(root: &Option<PathBuf>) -> Result<Vault> {
    let vault = Vault::open(vault_root(root))?;
    Ok(vault)
}

/// Carries ingest diagnostics through anyhow contexts without erasing typed errors.
#[derive(Debug)]
struct IngestWarnings(Vec<Value>);
impl std::fmt::Display for IngestWarnings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("command failed with ingest warnings")
    }
}

fn document_message(error: &wi_docs::Error) -> &'static str {
    match error {
        wi_docs::Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
            "The source file was not found. Check its name and location."
        }
        wi_docs::Error::Store(StoreError::NotFound { .. }) => {
            "The source file was not found. Check its name and location."
        }
        wi_docs::Error::Invalid(reason) if reason.contains("no page identifiers") => {
            "This is not a supported notes export. Export pages as Markdown and CSV, then import that folder or zip file."
        }
        wi_docs::Error::Conflict(_) => {
            "An imported file was edited. Its contents were kept; save your edits separately before importing again."
        }
        wi_docs::Error::Io(_) | wi_docs::Error::Store(StoreError::Io { .. }) => {
            "The source could not be read. Check that it is available and readable."
        }
        wi_docs::Error::Extraction(_) => {
            "The document could not be converted. Check that the file opens correctly and try again."
        }
        _ => {
            "The import could not finish. Check that the source is a supported document or a Markdown and CSV notes export."
        }
    }
}

fn ingest_warnings(report: &wi_store::ReconcileReport) -> Vec<Value> {
    report
        .failed
        .iter()
        .map(|failed| json!({"code":"ingest-failed", "path":failed.path, "message":"This file could not be read. Check that it is available and readable; automatic care will retry."}))
        .collect()
}

fn print_ingest_warnings(warnings: &[Value]) {
    for warning in warnings {
        let message = warning["message"]
            .as_str()
            .unwrap_or("unknown ingest failure");
        if let Some(path) = warning["path"].as_str() {
            eprintln!("warning: {path} is not in history yet: {message}");
        } else if warning["code"] == "index-degraded" {
            eprintln!("warning: {message}");
        } else {
            eprintln!("warning: auto-ingest skipped: {message}");
        }
    }
}

/// An open vault whose flush runs at most once per command.
pub(crate) struct Session {
    vault: Vault,
    flushed: std::cell::RefCell<Option<wi_store::FlushReport>>,
    warnings: std::cell::RefCell<Vec<Value>>,
}

impl std::ops::Deref for Session {
    type Target = Vault;
    fn deref(&self) -> &Vault {
        &self.vault
    }
}

impl Session {
    /// Open the vault and, when `ingest` is set, take in editor changes made since the last run.
    fn open(root: &Option<PathBuf>, ingest: bool) -> Result<Self> {
        let session = Self {
            vault: open(root)?,
            flushed: std::cell::RefCell::new(None),
            warnings: std::cell::RefCell::new(Vec::new()),
        };
        // A search index that could not be repaired on open is rebuilt from the files here.
        if session.vault.open_report().index_degraded.is_some()
            && let Err(error) = session.vault.reindex()
        {
            session.warnings.borrow_mut().push(json!({
                "code": "index-degraded",
                "path": null,
                "message": format!("search is slower for now because its index could not be rebuilt ({error}); it will be tried again next time"),
            }));
        }
        if ingest {
            session.auto_ingest();
        }
        Ok(session)
    }

    /// Take in editor changes. Problems are warnings: the command still runs, and each path that
    /// could not be taken in has a work request.
    fn auto_ingest(&self) {
        match self.vault.auto_ingest() {
            Ok(report) => {
                self.warnings.borrow_mut().extend(ingest_warnings(&report));
            }
            Err(error) => {
                self.warnings
                    .borrow_mut()
                    .push(json!({"code":error.code(), "path":null, "message":error.to_string()}));
            }
        }
    }

    /// After a command's own bulk commit, take in unrelated editor changes for the closing flush.
    pub(crate) fn ingest_after_bulk_commit(&self) {
        self.auto_ingest();
        *self.flushed.borrow_mut() = None;
    }

    /// Commit pending records. The first call flushes; later calls return its report.
    pub(crate) fn flush(&self) -> Result<wi_store::FlushReport> {
        if let Some(report) = self.flushed.borrow().as_ref() {
            return Ok(report.clone());
        }
        let report = self.vault.flush()?;
        *self.flushed.borrow_mut() = Some(report.clone());
        Ok(report)
    }
}

fn kv(items: &[String]) -> Result<Vec<(String, Value)>> {
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

/// Flush at exit of a one-shot mutating command (commits are off the save path).
pub(crate) fn flushed(session: &Session, mut value: Value) -> Result<Value> {
    let report = session.flush()?;
    if let Value::Object(map) = &mut value {
        map.insert("commits".into(), json!(report.commits.len()));
    }
    Ok(value)
}

fn write_output(vault: &Session, result: wi_store::WriteResult, verb: &str) -> Result<Output> {
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

fn run(cli: Cli) -> Result<Output> {
    let Cli {
        vault: root,
        json,
        command,
    } = cli;
    if matches!(
        command,
        Command::Mcp { .. }
            | Command::Note { .. }
            | Command::Init { .. }
            | Command::Watch { .. }
            | Command::Setup { .. }
            | Command::Uninstall
    ) {
        return run_standalone(&root, json, command);
    }
    // `wi commit` ingests on its own. `wi import` commits the originals it names together with
    // their sidecars in one bulk commit, then takes in other editor changes before the closing
    // flush. Every other command takes in editor changes first.
    let ingest = !matches!(
        command,
        Command::Commit | Command::Import { .. } | Command::Sync { .. }
    );
    let session = Session::open(&root, ingest)?;
    let reports_commits = reports_commits(&command);
    let result = run_in_vault(&session, command);
    finish(&session, result, reports_commits, json)
}

/// Commands whose JSON carries the number of commits their flush made.
fn reports_commits(command: &Command) -> bool {
    matches!(
        command,
        Command::Write { .. }
            | Command::Edit { .. }
            | Command::Move { .. }
            | Command::Archive { .. }
            | Command::Revert { .. }
            | Command::Capture(_)
            | Command::Adopt { .. }
            | Command::Sweep
            | Command::Work {
                command: lifecycle_cmd::WorkCommand::Close { .. }
                    | lifecycle_cmd::WorkCommand::OpenConflict { .. }
            }
    )
}

/// Flush exactly once, whatever the command did. A command error wins over a flush error.
fn finish(
    session: &Session,
    result: Result<Output>,
    reports_commits: bool,
    json_mode: bool,
) -> Result<Output> {
    let flush = session.flush();
    let outcome = match (result, flush) {
        (Ok(mut out), Ok(report)) => {
            if reports_commits && let Value::Object(map) = &mut out.json {
                map.entry("commits").or_insert(json!(report.commits.len()));
            }
            let warnings = session.warnings.borrow();
            if let Value::Object(map) = &mut out.json {
                map.insert("warnings".into(), json!(*warnings));
                print_ingest_warnings(&warnings);
            } else if json_mode {
                if !warnings.is_empty() {
                    eprintln!("{}", json!({"warnings": *warnings}));
                }
            } else {
                print_ingest_warnings(&warnings);
            }
            Ok(out)
        }
        (Ok(_), Err(flush_error)) => Err(flush_error),
        (Err(error), Err(flush_error)) => {
            eprintln!("warning: flush after failure also failed: {flush_error:#}");
            Err(error)
        }
        (Err(error), Ok(_)) => Err(error),
    };
    outcome.map_err(|error| {
        let warnings = session.warnings.borrow();
        if warnings.is_empty() {
            error
        } else {
            if !json_mode {
                print_ingest_warnings(&warnings);
            }
            error.context(IngestWarnings(warnings.clone()))
        }
    })
}

fn run_standalone(root: &Option<PathBuf>, json: bool, command: Command) -> Result<Output> {
    let root = root.clone();
    match command {
        Command::Setup { .. } | Command::Uninstall => {
            let home = setup_cmd::home()?;
            let root = setup_cmd::root(&root, &home);
            let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
            if let Command::Setup { primary, github } = command {
                let mut output = setup_cmd::setup(
                    &root,
                    &home,
                    &setup_cmd::executable()?,
                    codex_home.as_deref(),
                    &wi_export::service::SystemLaunchctl,
                    primary,
                )?;
                if github {
                    let message = sync_cmd::github(&root)?;
                    output.human.push_str(&format!("\n{message}"));
                    output.json["sync"] = json!(message);
                }
                Ok(output)
            } else {
                setup_cmd::uninstall(
                    &root,
                    &home,
                    codex_home.as_deref(),
                    &wi_export::service::SystemLaunchctl,
                )
            }
        }
        Command::Mcp {
            command: Some(McpCommand::Install { host, yes }),
        } => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is unset")?;
            let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
            let vault = std::fs::canonicalize(vault_root(&root))?;
            let plan = wi_export::install::plan_host(
                host.into(),
                &home,
                codex_home.as_deref(),
                &setup_cmd::executable()?,
                &vault,
            )?;
            // Keep the MCP stream silent; install is a separate CLI subcommand.
            if yes && !json {
                print!("{}", plan.diff);
            }
            let backup = if yes {
                let backup = plan.apply()?;
                let (vault, _) = Vault::init(&vault)?;
                let targets: Vec<_> = wi_export::skills::SkillTarget::for_host(host.into())
                    .into_iter()
                    .collect();
                wi_export::skills::enable(&vault, &targets)?;
                wi_export::skills::sync(&vault, &home, &targets, true)?;
                backup
            } else {
                None
            };
            Ok(Output {
                human: if yes {
                    format!(
                        "Applied: {}\nBackup: {}",
                        plan.changed,
                        backup
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "none".into())
                    )
                } else if plan.changed {
                    format!("{}Preview only; pass --yes to apply.", plan.diff)
                } else {
                    format!("No change: {} is already registered.", plan.path.display())
                },
                json: json!({"plan":plan,"applied":yes,"backup":backup}),
            })
        }
        Command::Mcp { command: None } => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(wi_mcp::serve_stdio(vault_root(&root)))?;
            Ok(Output {
                json: Value::Null,
                human: String::new(),
            })
        }
        Command::Note {
            command: NoteCommand::Parse { file },
        } => parse_note(&file),
        Command::Init { path } => {
            let target = path.or(root).unwrap_or_else(|| PathBuf::from("."));
            let (_, report) = Vault::init(&target)?;
            Ok(Output {
                human: format!(
                    "vault ready at {} (vault_id {}, {} commit(s))\nnext: wi service install --yes  (keeps this vault committed and indexed in the background)",
                    report.root.display(),
                    report.vault_id,
                    report.commits
                ),
                json: serde_json::to_value(&report)?,
            })
        }
        Command::Watch { duration } => {
            let vault = open(&root)?;
            // Keep host skills in step with procedures after each maintenance pass.
            let skills_root = vault.root().to_path_buf();
            let hooks = wi_store::WatchHooks {
                on_maintained: Some(Box::new(move |_report: &wi_store::MaintainReport| {
                    match Vault::open(&skills_root) {
                        Ok(vault) => {
                            sync_exports(&vault);
                            if vault.flush().is_err() {
                                eprintln!(
                                    "warning: Updated rules are saved; their history will finish automatically."
                                );
                            }
                        }
                        Err(e) => eprintln!("warning: skills were not updated: {e}"),
                    }
                })),
            };
            // Start the watcher first so edits made during the catch-up queue.
            let handle = vault.watch_with_hooks(wi_store::WatchTuning::default(), hooks)?;
            let mut sync = vault.start_sync()?;
            eprintln!(
                "watching {} (debounce {:?})",
                vault.root().display(),
                vault.debounce()
            );
            let reconciled = vault.reconcile()?;
            let warnings = ingest_warnings(&reconciled);
            print_ingest_warnings(&warnings);
            vault.flush()?;
            if !reconciled.ingested.is_empty() {
                eprintln!("reconciled {} file(s)", reconciled.ingested.len());
            }
            let deadline =
                duration.map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s));
            let log_home = wi_export::local::Targets::load(vault.root())?
                .map(|state| state.home)
                .or_else(|| setup_cmd::home().ok());
            let mut seen = 0usize;
            let mut rotation_warned = false;
            loop {
                let wait = std::time::Duration::from_millis(200);
                match handle.events().recv_timeout(wait) {
                    Ok(WatchEvent::Ready) => {}
                    Ok(WatchEvent::Ingest(ref report))
                        if matches!(
                            report.outcome,
                            wi_store::IngestOutcome::Unchanged
                                | wi_store::IngestOutcome::Skipped { .. }
                        ) => {}
                    Ok(event) => {
                        seen += 1;
                        if json {
                            println!("{}", serde_json::to_string(&event)?);
                        } else {
                            println!("{}", describe(&event));
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if sync.is_none() && vault.sync_status().is_ok_and(|status| status.configured) {
                    sync = vault.start_sync()?;
                }
                if cfg!(target_os = "macos")
                    && let Some(home) = &log_home
                    && wi_export::service::rotate_log(home, vault.vault_id()).is_err()
                    && !rotation_warned
                {
                    eprintln!(
                        "warning: The background care log could not be trimmed; background care will continue."
                    );
                    rotation_warned = true;
                }
                if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    break;
                }
            }
            handle.stop();
            if let Some(sync) = sync {
                sync.stop();
            }
            let report = vault.flush()?;
            Ok(Output {
                json: json!({"events": seen, "commits": report.commits.len(), "warnings": warnings}),
                human: String::new(),
            })
        }
        _ => unreachable!("vault commands are dispatched by run"),
    }
}

fn run_in_vault(vault: &Session, command: Command) -> Result<Output> {
    match command {
        Command::Sync { command } => sync_cmd::run(vault, command),
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
        Command::Doctor => {
            let mut report = vault.doctor()?;
            // Requests are durable ingress records; converted agent tasks are
            // reported separately rather than as stale owner warnings.
            report
                .checks
                .retain(|check| !matches!(check.name, "work" | "watcher"));
            let tasks = vault
                .work_list()?
                .iter()
                .filter(|w| w.actionable && w.state.as_deref() == Some("ready"))
                .count();
            report.checks.push(wi_store::Check {
                name: "work",
                status: "ok",
                detail: format!("{tasks} task(s) for your AI"),
                fix: None,
            });
            if cfg!(target_os = "macos") {
                let check = match setup_cmd::home().and_then(|home| {
                    Ok(wi_export::service::status(
                        &home,
                        vault.vault_id(),
                        &wi_export::service::SystemLaunchctl,
                    )?)
                }) {
                    Ok(status) => wi_store::Check {
                        name: "service",
                        status: if status.loaded { "ok" } else { "warn" },
                        detail: if status.loaded {
                            "Background care is running."
                        } else {
                            "Background care is not running."
                        }
                        .into(),
                        fix: (!status.loaded).then(|| "run wi setup".into()),
                    },
                    Err(_) => wi_store::Check {
                        name: "service",
                        status: "warn",
                        detail: "Background care could not be checked.".into(),
                        fix: Some("run wi setup".into()),
                    },
                };
                report.checks.push(check);
            }
            for check in &mut report.checks {
                if check.detail.contains("i/o error") || check.detail.contains("os error") {
                    check.detail = "Some files could not be read. Automatic care will retry when they are available.".into();
                }
                if check
                    .fix
                    .as_deref()
                    .is_some_and(|fix| fix.contains("wi commit"))
                {
                    check.fix = Some("Automatic care retries on the next command.".into());
                }
            }
            let mut human = format!("Notes: {}", report.root);
            for check in &report.checks {
                let (name, detail) = match (check.name, check.status) {
                    ("git", "ok") => ("history", "Saved history is up to date."),
                    ("manifest", "ok") => ("saves", "All saves are recorded."),
                    ("manifest", _) => (
                        "saves",
                        "Some saves are waiting to finish; automatic recovery will retry.",
                    ),
                    ("index", "ok") => ("search", "Search is up to date."),
                    ("index", _) => (
                        "search",
                        "Search is catching up automatically; notes remain safe.",
                    ),
                    ("temp-files", _) => (
                        "temporary files",
                        "Interrupted saves are being checked automatically.",
                    ),
                    _ => (check.name, check.detail.as_str()),
                };
                human.push_str(&format!("\n[{}] {}: {}", check.status, name, detail));
                if !matches!(check.name, "manifest" | "index" | "temp-files")
                    && let Some(fix) = &check.fix
                {
                    human.push_str(&format!("\n      Next: {fix}"));
                }
            }
            let mut json = serde_json::to_value(&report)?;
            json["work_requests"] = serde_json::to_value(vault.work_requests()?)?;
            json["index_degraded"] = serde_json::to_value(&vault.open_report().index_degraded)?;
            Ok(Output { json, human })
        }
        Command::Setup { .. }
        | Command::Uninstall
        | Command::Mcp { .. }
        | Command::Note { .. }
        | Command::Init { .. }
        | Command::Watch { .. } => unreachable!("standalone commands are dispatched by run"),
    }
}

/// Collapse whitespace to single spaces and cut at `SNIPPET_CHARS` characters.
fn one_line_snippet(raw: &str) -> String {
    let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= SNIPPET_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(SNIPPET_CHARS).collect();
    cut.push('…');
    cut
}

fn describe(event: &WatchEvent) -> String {
    match event {
        WatchEvent::Ready => "ready".into(),
        WatchEvent::Ingest(report) => format!("{}: {:?}", report.path, report.outcome),
        WatchEvent::Flushed { commits } => format!("committed {commits}"),
        WatchEvent::Error { message } => format!("error: {message}"),
    }
}

fn parse_note(file: &PathBuf) -> Result<Output> {
    let bytes = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
    let note = Note::parse(&bytes).context("parse note")?;
    let keys: Vec<_> = note
        .frontmatter()
        .as_ref()
        .map(|f| f.keys().collect())
        .unwrap_or_default();
    let lifecycle = note.lifecycle();
    let observations = note.observations();
    let relations = note.relations();
    let wikilinks = note.wikilinks();
    let json = serde_json::json!({ "frontmatter_keys": keys, "lifecycle_type": lifecycle.lifecycle_type,
        "observations": observations, "relations": relations, "wikilinks": wikilinks, "diagnostics": note.diagnostics() });
    let mut human = format!("Frontmatter keys: {}\n", keys.join(", "));
    human.push_str(&format!("Lifecycle: {:?}\n", lifecycle.lifecycle_type));
    human.push_str(&format!(
        "Observations: {}\n",
        serde_json::to_string_pretty(&observations)?
    ));
    human.push_str(&format!(
        "Relations: {}\n",
        serde_json::to_string_pretty(&relations)?
    ));
    human.push_str(&format!(
        "Wikilinks: {}\n",
        serde_json::to_string_pretty(&wikilinks)?
    ));
    for diagnostic in note.diagnostics() {
        human.push_str(&format!("Diagnostic: {}\n", diagnostic.message));
    }
    Ok(Output { json, human })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn missing_help(command: &clap::Command, trail: &str, out: &mut Vec<String>) {
        for arg in command.get_arguments() {
            let id = arg.get_id().as_str();
            if id == "help" || id == "version" {
                continue;
            }
            let has_help = arg
                .get_help()
                .is_some_and(|h| !h.to_string().trim().is_empty());
            if !has_help {
                out.push(format!("{trail} {id}"));
            }
        }
        for sub in command.get_subcommands() {
            if sub.get_name() == "help" {
                continue;
            }
            missing_help(sub, &format!("{trail} {}", sub.get_name()), out);
        }
    }

    #[test]
    fn every_argument_has_help() {
        let mut missing = Vec::new();
        missing_help(&Cli::command(), "wi", &mut missing);
        assert!(missing.is_empty(), "arguments without help: {missing:?}");
    }

    #[test]
    fn snippet_is_one_line_and_truncated() {
        assert_eq!(one_line_snippet("a\n  b\tc"), "a b c");
        assert_eq!(one_line_snippet("  "), "");
        let long = "é".repeat(SNIPPET_CHARS + 5);
        let cut = one_line_snippet(&long);
        assert_eq!(cut.chars().count(), SNIPPET_CHARS + 1);
        assert!(cut.ends_with('…'));
    }
}
