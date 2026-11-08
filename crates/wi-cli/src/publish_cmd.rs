//! Explicit and background publication of committed notes into a
//! read-only Notion view. No capture, search, read or MCP path reaches this
//! module. State lives in this device's data directory only, so only the
//! device where publishing was set up ever publishes.
use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wi_notion::{MapStore, NoteSource, PublishNote, PublishOptions, TokenSource};
use wi_store::Vault;

use super::Output;

/// Keychain service name; the account is the vault id.
const KEYCHAIN_SERVICE: &str = "wiki-island.notion";

#[derive(Subcommand)]
pub(crate) enum PublishCommand {
    /// Publish committed notes as a read-only Notion view.
    Notion {
        #[command(subcommand)]
        command: Option<NotionCommand>,
    },
}

#[derive(Subcommand)]
pub(crate) enum NotionCommand {
    /// Choose the parent page and store the integration token in the keychain.
    /// The token is read from standard input or a hidden prompt.
    Setup {
        /// Notion page URL or page id that will hold the published pages.
        #[arg(long)]
        parent: String,
    },
    /// Show publishing state without contacting Notion.
    Status,
}

pub(crate) fn run(vault: &super::Session, command: PublishCommand) -> Result<Output> {
    let PublishCommand::Notion { command } = command;
    match command {
        Some(NotionCommand::Setup { parent }) => {
            let token = read_token()?;
            setup_with(vault, &parent, &token, &KeychainStore)?;
            Ok(Output {
                human: "Notion publishing is set up on this device; run wi publish notion to publish now.".into(),
                json: status(vault)?,
            })
        }
        Some(NotionCommand::Status) => {
            let value = status(vault)?;
            let human = if value["configured"] != true {
                "Notion publishing is not set up on this device; run wi publish notion setup --parent <page>.".to_owned()
            } else if value["failures"].as_u64().unwrap_or(0) > 0 {
                "Notion publishing will retry automatically; your notes are unaffected.".to_owned()
            } else {
                format!(
                    "Notion publishing is set up; {} page(s) published.",
                    value["published"]
                )
            };
            Ok(Output { human, json: value })
        }
        None => {
            // Publish what is committed, including this command's own ingest.
            vault.flush()?;
            let account = vault.vault_id().to_owned();
            let report = publish_now(
                vault,
                &wi_notion::KeychainTokenSource { account },
                None,
                false,
            )?;
            let human = format!(
                "Notion view updated. {} created, {} updated, {} archived, {} unchanged, {} failed.",
                report["created"].as_array().map_or(0, Vec::len),
                report["updated"].as_array().map_or(0, Vec::len),
                report["archived"].as_array().map_or(0, Vec::len),
                report["unchanged"],
                report["failed"].as_array().map_or(0, Vec::len),
            );
            Ok(Output {
                human,
                json: report,
            })
        }
    }
}

/// Stores a secret for an account. Implementations never log the secret.
pub(crate) trait SecretStore {
    fn save(&self, account: &str, secret: &str) -> Result<()>;
}

/// macOS keychain through `/usr/bin/security`. The token goes over the
/// child's standard input (interactive command mode), never its arguments.
struct KeychainStore;

impl SecretStore for KeychainStore {
    fn save(&self, account: &str, secret: &str) -> Result<()> {
        if !cfg!(target_os = "macos") {
            bail!(
                "Notion publishing stores its token in the macOS keychain; this device has none."
            );
        }
        let mut child = std::process::Command::new("/usr/bin/security")
            .arg("-i")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| anyhow!("The keychain could not be opened; the token was not saved."))?;
        let line =
            format!("add-generic-password -U -s {KEYCHAIN_SERVICE} -a {account} -w \"{secret}\"\n");
        let wrote = child
            .stdin
            .take()
            .map(|mut stdin| stdin.write_all(line.as_bytes()).is_ok())
            .unwrap_or(false);
        let status = child.wait();
        if !wrote || !status.is_ok_and(|s| s.success()) {
            bail!("The keychain did not accept the token; it was not saved.");
        }
        Ok(())
    }
}

/// Read the token from piped standard input, or a hidden terminal prompt.
fn read_token() -> Result<String> {
    let mut text = String::new();
    if std::io::stdin().is_terminal() {
        eprint!("Notion integration token (hidden): ");
        let hidden = std::process::Command::new("/bin/stty")
            .arg("-echo")
            .status()
            .is_ok_and(|s| s.success());
        let read = std::io::stdin().read_line(&mut text);
        if hidden {
            let _ = std::process::Command::new("/bin/stty").arg("echo").status();
        }
        eprintln!();
        read?;
    } else {
        std::io::stdin()
            .take(4096)
            .read_to_string(&mut text)
            .map_err(|_| anyhow!("The token could not be read from standard input."))?;
    }
    Ok(text.trim().to_owned())
}

/// Notion page ids are 32 hex digits, optionally dashed, at the end of a
/// page URL's last path segment.
pub(crate) fn parse_parent(input: &str) -> Result<String> {
    let path = input.trim().split(['?', '#']).next().unwrap_or_default();
    let last = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let hex: String = last.chars().filter(|c| *c != '-').collect();
    let tail = hex.len().checked_sub(32).map(|start| &hex[start..]);
    let Some(id) = tail.filter(|t| t.bytes().all(|b| b.is_ascii_hexdigit())) else {
        bail!("Use the Notion page link or its 32-character page id.");
    };
    let id = id.to_ascii_lowercase();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &id[0..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..32]
    ))
}

fn valid_token(token: &str) -> bool {
    (16..=512).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn state_dir(vault: &Vault) -> Result<PathBuf> {
    Ok(vault.device_state_dir()?.join("notion"))
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(|_| {
            anyhow!("The saved publishing state cannot be read; set up publishing again.")
        })?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    use wi_history::durable::{self, Durability};
    let dir = path.parent().ok_or_else(|| anyhow!("invalid state path"))?;
    durable::create_dir_durable(dir, Durability::Standard)?;
    durable::write_atomic(
        path,
        &serde_json::to_vec_pretty(value)?,
        Durability::Standard,
    )?;
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Save the token, then the parent. The token never reaches the vault,
/// the data directory, logs or output.
pub(crate) fn setup_with(
    vault: &Vault,
    parent: &str,
    token: &str,
    secrets: &dyn SecretStore,
) -> Result<()> {
    let parent = parse_parent(parent)?;
    if !valid_token(token) {
        bail!(
            "That does not look like a Notion integration token; copy it again from the integration settings."
        );
    }
    let dir = state_dir(vault)?;
    if let Some(config) = read_json(&dir.join("config.json"))?
        && config["parent_page_id"] != parent.as_str()
        && dir.join("map.json").exists()
    {
        bail!(
            "Notion publishing already uses another parent page on this device; remove its publishing state to change it."
        );
    }
    secrets.save(vault.vault_id(), token)?;
    write_json(
        &dir.join("config.json"),
        &json!({"version": 1, "parent_page_id": parent}),
    )
}

/// Configured on this device: a parent page id, else `None`.
pub(crate) fn configured(vault: &Vault) -> Result<Option<String>> {
    Ok(read_json(&state_dir(vault)?.join("config.json"))?
        .and_then(|c| c["parent_page_id"].as_str().map(str::to_owned)))
}

pub(crate) fn status(vault: &Vault) -> Result<Value> {
    let dir = state_dir(vault)?;
    let parent = configured(vault)?;
    let state = read_json(&dir.join("state.json"))?.unwrap_or_else(|| json!({}));
    let published = match MapStore::open(dir.join("map.json")) {
        Ok(map) => map.entries().values().filter(|e| !e.archived).count(),
        Err(_) => 0,
    };
    Ok(json!({
        "configured": parent.is_some(),
        "parent_page_id": parent,
        "published": published,
        "failures": state["failures"].as_u64().unwrap_or(0),
        "next_attempt_ms": state["next_attempt_ms"].as_u64().unwrap_or(0),
        "last_success_ms": state["last_success_ms"],
        "last_error": state["last_error"],
        "health": state["health"],
    }))
}

/// The committed branch tip, read without any engine lock.
struct Committed<'a>(&'a Vault);

impl NoteSource for Committed<'_> {
    fn notes(&self) -> wi_notion::Result<Vec<PublishNote>> {
        let notes = self
            .0
            .committed_notes()
            .map_err(|e| wi_notion::Error::Source(e.to_string()))?;
        Ok(notes
            .into_iter()
            .map(|n| PublishNote {
                rel_path: n.rel_path,
                bytes: n.bytes,
                permalink: n.permalink,
                title: n.title,
                archived: n.archived,
            })
            .collect())
    }
}

/// One publication attempt. `background` honors the persisted backoff and
/// skips (with `deferred`) when another attempt holds the device lock.
/// `api_base` overrides the service address for loopback tests only.
pub(crate) fn publish_now(
    vault: &Vault,
    tokens: &dyn TokenSource,
    api_base: Option<&str>,
    background: bool,
) -> Result<Value> {
    let Some(parent) = configured(vault)? else {
        bail!(
            "Notion publishing is not set up on this device; run wi publish notion setup --parent <page>."
        );
    };
    let dir = state_dir(vault)?;
    std::fs::create_dir_all(&dir)?;
    // One publisher per device; the map is never shared.
    let lock = File::create(dir.join("lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) if background => {
            return Ok(json!({"deferred": true}));
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("Another Notion publish is running on this device; try again shortly.")
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    }
    let state_path = dir.join("state.json");
    let mut state = read_json(&state_path)?.unwrap_or_else(|| json!({}));
    let now = now_ms();
    if background && now < state["next_attempt_ms"].as_u64().unwrap_or(0) {
        return Ok(json!({"deferred": true}));
    }
    let mut options = PublishOptions::new(parent);
    if let Some(base) = api_base {
        options.api_base_url = base.to_owned();
    }
    let result = MapStore::open(dir.join("map.json"))
        .and_then(|mut map| wi_notion::publish(&Committed(vault), tokens, &mut map, &options));
    let now = now_ms();
    let failed = match &result {
        Ok(report) => !report.failed.is_empty(),
        Err(_) => true,
    };
    state["health"] = Value::Null;
    if failed {
        let failures = state["failures"].as_u64().unwrap_or(0).saturating_add(1);
        let first = state["failing_since_ms"].as_u64().unwrap_or(now);
        let seconds = 60u64
            .saturating_mul(1u64 << failures.saturating_sub(1).min(6))
            .min(3600);
        state["failures"] = json!(failures);
        state["failing_since_ms"] = json!(first);
        state["next_attempt_ms"] = json!(now.saturating_add(seconds * 1000));
        state["last_error"] = json!(match &result {
            Ok(_) => "Some notes could not be published; they will be retried.".to_owned(),
            Err(e) => e.to_string(),
        });
        if now.saturating_sub(first) >= 24 * 60 * 60 * 1000 && state["health_reported"] != true {
            state["health"] = json!(
                "Your notes are safe, but the Notion view has not updated for a day; check the integration token and page access."
            );
            state["health_reported"] = json!(true);
        }
    } else {
        state = json!({"failures": 0, "next_attempt_ms": 0, "last_success_ms": now});
    }
    write_json(&state_path, &state)?;
    drop(lock);
    let report = result?;
    let failures: Vec<Value> = report
        .failed
        .iter()
        .map(|f| json!({"permalink": f.permalink, "reason": f.reason}))
        .collect();
    Ok(json!({
        "deferred": false,
        "created": report.created,
        "updated": report.updated,
        "archived": report.archived,
        "unchanged": report.unchanged.len(),
        "failed": failures,
        "warnings": report.warnings,
        "health": state["health"],
    }))
}

/// Background publisher beside the sync thread. It starts only where
/// publishing was set up, waits until the committed tip has been still for
/// a minute (sync and flushes settled), and also runs every five minutes.
pub(crate) struct PublishHandle {
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PublishHandle {
    pub(crate) fn stop(mut self) {
        self.finish();
    }
    fn finish(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PublishHandle {
    fn drop(&mut self) {
        self.finish();
    }
}

pub(crate) fn start(vault: &Vault) -> Result<Option<PublishHandle>> {
    let root = vault.root().to_path_buf();
    let tokens = wi_notion::KeychainTokenSource {
        account: vault.vault_id().to_owned(),
    };
    start_with(
        vault,
        Timing::default(),
        None,
        Box::new(tokens),
        move || Vault::open(&root).ok(),
    )
}

#[derive(Clone, Copy)]
pub(crate) struct Timing {
    pub settle: Duration,
    pub period: Duration,
    pub tick: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            settle: Duration::from_secs(60),
            period: Duration::from_secs(300),
            tick: Duration::from_secs(1),
        }
    }
}

pub(crate) fn start_with(
    vault: &Vault,
    timing: Timing,
    api_base: Option<String>,
    tokens: Box<dyn TokenSource + Send>,
    open: impl FnOnce() -> Option<Vault> + Send + 'static,
) -> Result<Option<PublishHandle>> {
    if configured(vault)?.is_none() {
        return Ok(None);
    }
    let (stop, receive) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        // A thread-owned handle; engine locks are never held across HTTP.
        let Some(vault) = open() else {
            return;
        };
        let mut last: Option<Instant> = None;
        let mut observed = None;
        let mut changed = Instant::now();
        loop {
            let tip = vault.generation().ok();
            if observed != Some(tip) {
                observed = Some(tip);
                changed = Instant::now();
            }
            let now = Instant::now();
            let settled = now.duration_since(changed) >= timing.settle;
            let due = last.is_none_or(|at| now.duration_since(at) >= timing.period);
            let fresh = last.is_some_and(|at| at >= changed);
            if settled && (due || !fresh) {
                match publish_now(&vault, tokens.as_ref(), api_base.as_deref(), true) {
                    Ok(report) if report["deferred"] == true => {}
                    Ok(report) => {
                        if let Some(line) = report["health"].as_str() {
                            eprintln!("{line}");
                        }
                    }
                    Err(_) => {
                        if let Ok(status) = status(&vault)
                            && let Some(line) = status["health"].as_str()
                        {
                            eprintln!("{line}");
                        }
                    }
                }
                last = Some(Instant::now());
            }
            if !matches!(
                receive.recv_timeout(timing.tick),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                break;
            }
        }
    });
    Ok(Some(PublishHandle {
        stop,
        thread: Some(thread),
    }))
}

#[cfg(test)]
mod tests;
