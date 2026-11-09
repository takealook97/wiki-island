//! Commands that run without opening a vault session.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use wi_note::Note;
use wi_store::{Vault, WatchEvent};

use super::Output;
use super::args::{Command, McpCommand, NoteCommand};
use super::session::{ingest_warnings, open, print_ingest_warnings, vault_root};
use super::{publish_cmd, service_cmd, setup_cmd, sync_cmd};

/// Refresh the host skill copies of this vault's procedures. Problems are warnings: notes and
/// maintenance are unaffected, and the next pass tries again.
pub(crate) fn sync_exports(vault: &Vault) {
    if let Err(e) = wi_export::Regenerator::default().poll(vault, false) {
        eprintln!("warning: rules were not updated: {e}");
    }
    if let Err(e) = wi_export::skills::sync_recorded(vault) {
        eprintln!("warning: skills were not updated: {e}");
    }
}

pub(crate) fn run_standalone(
    root: &Option<PathBuf>,
    json: bool,
    command: Command,
) -> Result<Output> {
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
                    wi_export::service::default_runner(),
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
                    wi_export::service::default_runner(),
                )
            }
        }
        Command::Mcp {
            command: Some(McpCommand::Install { host, yes }),
        } => {
            let home = setup_cmd::integration_home()?;
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
            let mut publisher = publish_cmd::start(&vault)?;
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
            let log_home = wi_export::local::Targets::load(&vault)?
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
                if publisher.is_none() && publish_cmd::configured(&vault).is_ok_and(|p| p.is_some())
                {
                    publisher = publish_cmd::start(&vault)?;
                }
                if let Some(home) = &log_home
                    && !service_cmd::rotate_log(home, vault.vault_id())
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
            if let Some(publisher) = publisher {
                publisher.stop();
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
