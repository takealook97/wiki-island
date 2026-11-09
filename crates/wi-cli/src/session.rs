//! Open vault sessions: ingest warnings and the single closing flush.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Value, json};
use wi_store::Vault;

use super::Output;
use super::errors::IngestWarnings;
use super::setup_cmd;

pub(crate) fn vault_root(cli_vault: &Option<PathBuf>) -> PathBuf {
    cli_vault.clone().unwrap_or_else(|| {
        if !std::path::Path::new(".wiki-island/config.toml").is_file()
            && let Ok(home) = setup_cmd::home()
        {
            let default = home.join("wiki-island");
            if default.join(".wiki-island/config.toml").is_file() {
                return default;
            }
        }
        PathBuf::from(".")
    })
}

pub(crate) fn open(root: &Option<PathBuf>) -> Result<Vault> {
    let vault = Vault::open(vault_root(root))?;
    Ok(vault)
}

pub(crate) fn ingest_warnings(report: &wi_store::ReconcileReport) -> Vec<Value> {
    report
        .failed
        .iter()
        .map(|failed| json!({"code":"ingest-failed", "path":failed.path, "message":"This file could not be read. Check that it is available and readable; automatic care will retry."}))
        .collect()
}

pub(crate) fn print_ingest_warnings(warnings: &[Value]) {
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
    pub(crate) warnings: std::cell::RefCell<Vec<Value>>,
}

impl std::ops::Deref for Session {
    type Target = Vault;
    fn deref(&self) -> &Vault {
        &self.vault
    }
}

impl Session {
    /// Open the vault and, when `ingest` is set, take in editor changes made since the last run.
    pub(crate) fn open(root: &Option<PathBuf>, ingest: bool) -> Result<Self> {
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

/// Flush at exit of a one-shot mutating command (commits are off the save path).
pub(crate) fn flushed(session: &Session, mut value: Value) -> Result<Value> {
    let report = session.flush()?;
    if let Value::Object(map) = &mut value {
        map.insert("commits".into(), json!(report.commits.len()));
    }
    Ok(value)
}

/// Flush exactly once, whatever the command did. A command error wins over a flush error.
pub(crate) fn finish(
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
