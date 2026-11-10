#![forbid(unsafe_code)]

mod adoption_view;
mod args;
mod brief;
mod dispatch;
mod display_path;
mod doctor;
mod error_codes;
mod error_text;
mod errors;
mod import_paths;
mod lifecycle_cmd;
mod publish_cmd;
mod service_cmd;
mod session;
mod setup_cmd;
#[path = "cli/standalone.rs"]
mod standalone;
mod sync_cmd;
#[path = "cli/vault.rs"]
mod vault_cmd;

use anyhow::Result;
use serde_json::{Value, json};
use wi_store::StoreError;

use args::{Command, parse_args};
use dispatch::run;
// Sibling command modules reach these through `super::`.
pub(crate) use args::ServiceCommand;
use errors::{IngestWarnings, Root, hint, plain_error};
pub(crate) use session::{Session, flushed};

pub(crate) struct Output {
    pub(crate) json: Value,
    pub(crate) human: String,
}

pub(crate) fn main() -> Result<()> {
    let cli = parse_args();
    let json = cli.json && !matches!(cli.command, Command::Mcp { command: None });
    let ndjson = matches!(cli.command, Command::Watch { .. });
    let root = Root::new(&session::vault_root(&cli.vault));
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
            let message = plain_error(&error, &root);
            if json {
                let warnings = error
                    .downcast_ref::<IngestWarnings>()
                    .map(|w| w.0.as_slice())
                    .unwrap_or(&[]);
                let body = json!({"error": {"code": code, "message": message, "hint": hint}, "warnings": warnings});
                println!(
                    "{}",
                    if ndjson {
                        serde_json::to_string(&body)?
                    } else {
                        serde_json::to_string_pretty(&body)?
                    }
                );
            }
            eprintln!("error: {message}");
            if let Some(hint) = hint {
                eprintln!("hint: {hint}");
            }
            if debug_requested() {
                eprintln!("detail: {error:#}");
            }
            std::process::exit(1);
        }
    }
}

/// `WI_DEBUG=1` adds the full original error chain on a `detail:` line.
fn debug_requested() -> bool {
    std::env::var_os("WI_DEBUG").is_some_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
    use super::args::Cli;
    use super::vault_cmd::{SNIPPET_CHARS, one_line_snippet};
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
