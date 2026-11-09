//! Command-line arguments and their parsing.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;

use super::lifecycle_cmd;
use super::{publish_cmd, sync_cmd};

#[derive(Parser)]
#[command(name = "wi", version, about = "wiki-island personal knowledge engine")]
pub(crate) struct Cli {
    /// Notes folder (default: $WI_VAULT, else ~/wiki-island; uses the current folder when it is a vault).
    #[arg(long, global = true, env = "WI_VAULT")]
    pub(crate) vault: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum InstallHost {
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
pub(crate) enum RulesHost {
    Claude,
    Codex,
    Gemini,
    All,
}
#[derive(Subcommand)]
pub(crate) enum McpCommand {
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
pub(crate) enum ServiceCommand {
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
pub(crate) enum ExportCommand {
    Rules {
        /// Host; omit to generate all three fragments.
        #[arg(long, value_enum)]
        host: Option<RulesHost>,
    },
}

#[derive(Subcommand)]
pub(crate) enum SkillsCommand {
    /// Update the installed copies of your procedures.
    Sync,
    /// Preview which procedure copies need updating.
    Status,
}
#[derive(Subcommand)]
pub(crate) enum Command {
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
    /// Publish committed notes to a read-only view (Notion).
    Publish {
        #[command(subcommand)]
        command: publish_cmd::PublishCommand,
    },
    /// Remove wiki-island connections and background care; keep your notes.
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
pub(crate) enum NoteCommand {
    Parse {
        /// Markdown file to parse.
        file: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum Mode {
    Text,
    Title,
    Permalink,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum OcrMode {
    Auto,
    Off,
    Required,
}

/// Parse the arguments. With `--json`, a usage error is printed as a JSON
/// error object (exit 2) instead of clap's text.
pub(crate) fn parse_args() -> Cli {
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
