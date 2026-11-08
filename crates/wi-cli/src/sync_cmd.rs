//! Explicit remote setup and synchronization. No MCP path calls these.
use super::{Output, Session};
use anyhow::{Result, bail};
use clap::Subcommand;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Subcommand)]
pub(crate) enum SyncCommand {
    /// Remember the address of an existing git remote.
    Setup {
        /// HTTPS, SSH or local git remote address.
        url: String,
    },
    /// Show connection and retry status without contacting the remote.
    Status,
}

pub(crate) fn run(vault: &Session, command: Option<SyncCommand>) -> Result<Output> {
    match command {
        Some(SyncCommand::Setup { url }) => {
            let status = vault.sync_setup(&url)?;
            Ok(Output {
                human: "Sync is connected; run wi sync to synchronize now.".into(),
                json: serde_json::to_value(status)?,
            })
        }
        Some(SyncCommand::Status) => {
            let status = vault.sync_status()?;
            let human = if !status.configured {
                "Sync is not connected; run wi sync setup <url>."
            } else if status.failures > 0 {
                "Notes are saved here; sync will retry automatically."
            } else {
                "Sync is connected."
            };
            Ok(Output {
                human: human.into(),
                json: serde_json::to_value(status)?,
            })
        }
        None => {
            let report = match vault.sync() {
                Ok(report) => report,
                Err(error) => {
                    if let Ok(status) = vault.sync_status()
                        && let Some(line) = status.health
                    {
                        eprintln!("{line}");
                    }
                    return Err(error.into());
                }
            };
            let human = format!(
                "Notes synchronized. {} changed, {} conflict copy(s), {} large file(s) excluded.",
                report.changed.len(),
                report.conflicts.len(),
                report.excluded.len()
            );
            Ok(Output {
                human,
                json: serde_json::to_value(report)?,
            })
        }
    }
}

/// This function is reached only by an explicit `wi setup --github`.
/// Existing repositories are reused; no force push or history rewrite.
pub(crate) fn github(root: &Path) -> Result<String> {
    github_with(root, &gh)
}

fn gh(args: &[&str]) -> Option<std::process::Output> {
    let mut child = Command::new("gh")
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().ok()?.is_some() {
            return child.wait_with_output().ok();
        }
        if start.elapsed() >= std::time::Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn github_with(
    root: &Path,
    run: &dyn Fn(&[&str]) -> Option<std::process::Output>,
) -> Result<String> {
    let vault = wi_store::Vault::open(root)?;
    if vault.storage_mode() != wi_store::StorageMode::Git {
        bail!("This folder uses file sync; remote git sync needs a git-mode notes folder.");
    }
    let fallback = "Connect an existing private repository with: wi sync setup <url>.";
    let Some(output) = run(&["api", "user", "--jq", ".login"]) else {
        return Ok(fallback.into());
    };
    if !output.status.success() {
        return Ok(fallback.into());
    }
    let login = String::from_utf8(output.stdout)?.trim().to_owned();
    if login.is_empty()
        || !login
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        bail!("Your account name could not be read; connect with wi sync setup <url>.");
    }
    let repo = format!("{login}/wiki-island-notes");
    let Some(exists) = run(&["repo", "view", &repo, "--json", "isPrivate"]) else {
        return Ok(fallback.into());
    };
    if exists.status.success() {
        let value: serde_json::Value = serde_json::from_slice(&exists.stdout)?;
        if value["isPrivate"] != true {
            bail!(
                "The notes repository is public; connect a private remote with wi sync setup <url>."
            );
        }
    } else {
        let Some(created) = run(&["repo", "create", &repo, "--private"]) else {
            return Ok(fallback.into());
        };
        if !created.status.success() {
            return Ok(fallback.into());
        }
    }
    vault.sync_setup(&format!("https://github.com/{repo}.git"))?;
    Ok("Private remote sync is connected; background care will synchronize your notes.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    fn output(ok: bool, text: &str) -> Option<std::process::Output> {
        Some(std::process::Output {
            status: std::process::ExitStatus::from_raw(if ok {
                0
            } else if cfg!(unix) {
                256
            } else {
                1
            }),
            stdout: text.as_bytes().to_vec(),
            stderr: vec![],
        })
    }
    fn fixture() -> (tempfile::TempDir, wi_store::Vault) {
        let temp = tempfile::tempdir().unwrap();
        let (vault, _) = wi_store::Vault::init_placed(
            temp.path().join("notes"),
            wi_store::OpenOptions {
                index_path: Some(temp.path().join("index.sqlite3")),
            },
            wi_store::Placement {
                storage: Some(wi_store::StorageMode::Git),
                data_root: Some(temp.path().join("data")),
            },
        )
        .unwrap();
        (temp, vault)
    }
    #[test]
    fn unavailable_auth_prints_one_connection_command_without_configuring_sync() {
        let (_temp, vault) = fixture();
        for reply in [None, output(false, "")] {
            let message = github_with(vault.root(), &|_| reply.clone()).unwrap();
            assert!(message.contains("wi sync setup <url>"));
            assert!(!vault.sync_status().unwrap().configured);
        }
    }
    #[test]
    fn private_repository_is_created_or_reused_and_public_is_refused() {
        for existing in [None, Some(true), Some(false)] {
            let (_temp, vault) = fixture();
            let calls = std::cell::RefCell::new(Vec::new());
            let result = github_with(vault.root(), &|args| {
                calls.borrow_mut().push(args.join(" "));
                match args[0..2] {
                    ["api", "user"] => output(true, "sample-owner\n"),
                    ["repo", "view"] => output(
                        existing.is_some(),
                        if existing == Some(true) {
                            "{\"isPrivate\":true}"
                        } else {
                            "{\"isPrivate\":false}"
                        },
                    ),
                    ["repo", "create"] => {
                        assert!(args.contains(&"--private"));
                        output(true, "")
                    }
                    _ => panic!("unexpected command"),
                }
            });
            assert_eq!(result.is_ok(), existing != Some(false));
            assert_eq!(
                vault.sync_status().unwrap().configured,
                existing != Some(false)
            );
            assert_eq!(
                calls
                    .borrow()
                    .iter()
                    .filter(|c| c.starts_with("repo create "))
                    .count(),
                usize::from(existing.is_none())
            );
        }
    }
}
