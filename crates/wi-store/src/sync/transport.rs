//! Sync configuration files and the git transport.

use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use wi_history::{PublisherLock, durable};

use crate::error::{Result, StoreError};
use crate::vault::Vault;

use super::*;

impl Vault {
    pub(super) fn sync_file(&self, name: &str) -> PathBuf {
        self.history.wi_dir().join(name)
    }

    pub(super) fn load_sync<T: serde::de::DeserializeOwned>(
        &self,
        name: &str,
    ) -> Result<Option<T>> {
        let path = self.sync_file(name);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| invalid("The saved sync state cannot be read; your notes are safe.")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::io(path, e)),
        }
    }
    pub(super) fn save_sync(&self, name: &str, value: &impl Serialize) -> Result<()> {
        let path = self.sync_file(name);
        let bytes =
            serde_json::to_vec(value).map_err(|_| invalid("The sync state could not be saved."))?;
        durable::write_atomic(&path, &bytes, self.history.durability())?;
        Ok(())
    }

    /// Configure an existing remote. This does not contact it.
    pub fn sync_setup(&self, url: &str) -> Result<SyncStatus> {
        if self.storage_mode() != wi_history::StorageMode::Git {
            return Err(invalid(
                "This folder already uses file sync; git remote sync needs git mode.",
            ));
        }
        if url.is_empty()
            || url.starts_with('-')
            || url.chars().any(char::is_control)
            || url.starts_with("ext:")
            || url.contains("::")
            || (url.contains("://")
                && !["https://", "ssh://", "file://"]
                    .iter()
                    .any(|scheme| url.starts_with(scheme)))
        {
            return Err(invalid("Use an HTTPS, SSH or local git remote address."));
        }
        let branch = "main".to_owned();
        let _guard = PublisherLock::acquire(&self.history)?;
        self.save_sync(
            CONFIG,
            &Config {
                url: url.into(),
                branch,
            },
        )?;
        self.save_sync(STATE, &SyncStatus::default())?;
        self.sync_status()
    }

    pub fn sync_status(&self) -> Result<SyncStatus> {
        let mut status = self.load_sync::<SyncStatus>(STATE)?.unwrap_or_default();
        status.configured = self.load_sync::<Config>(CONFIG)?.is_some();
        Ok(status)
    }

    /// A bounded, noninteractive subprocess. Output is spooled to unique
    /// local files so a verbose transport cannot deadlock a full pipe.
    pub(super) fn transport(&self, args: &[&str]) -> Result<(bool, Vec<u8>, i32)> {
        let id = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
        let out_path = self.sync_file(&format!("transport-{id}.out"));
        let result = (|| {
            let out = File::create(&out_path).map_err(|e| StoreError::io(&out_path, e))?;
            let mut command = Command::new("git");
            command
                .arg("--git-dir")
                .arg(self.history.git_dir())
                .arg("--work-tree")
                .arg(self.root());
            #[cfg(not(windows))]
            command
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "protocol.ext.allow=never",
                    "-c",
                    "credential.interactive=false",
                ])
                .env("GIT_ASKPASS", "/usr/bin/false")
                .env("SSH_ASKPASS", "/usr/bin/false");
            #[cfg(windows)]
            {
                // D15: an empty hooks directory under the engine dir instead
                // of /dev/null, and askpass helpers that do not exist, so
                // git fails instead of prompting.
                let hooks = self.sync_file("no-hooks");
                std::fs::create_dir_all(&hooks).map_err(|e| StoreError::io(&hooks, e))?;
                let mut hooks_path = std::ffi::OsString::from("core.hooksPath=");
                hooks_path.push(hooks.as_os_str());
                let askpass = self.sync_file("no-askpass").join("askpass.exe");
                command
                    .arg("-c")
                    .arg(hooks_path)
                    .args([
                        "-c",
                        "protocol.ext.allow=never",
                        "-c",
                        "credential.interactive=never",
                    ])
                    .env("GIT_ASKPASS", &askpass)
                    .env("SSH_ASKPASS", &askpass);
            }
            command
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_OBJECT_DIRECTORY")
                .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
                .env_remove("GIT_NAMESPACE")
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GCM_INTERACTIVE", "Never")
                .env("GCM_GUI_PROMPT", "false")
                .env(
                    "GIT_SSH_COMMAND",
                    "ssh -oBatchMode=yes -oConnectTimeout=15 -oStrictHostKeyChecking=yes",
                )
                .stdin(Stdio::null())
                .stdout(if args.first() == Some(&"ls-remote") {
                    Stdio::from(out)
                } else {
                    Stdio::null()
                })
                .stderr(Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            #[cfg(not(windows))]
            let mut child = command
                .spawn()
                .map_err(|_| invalid("Git is unavailable; install git to sync your notes."))?;
            // D14: the whole transport tree runs in a kill-on-close job.
            #[cfg(windows)]
            let (mut child, job) = wi_sys::process::spawn_in_job(&mut command)
                .map_err(|_| invalid("Git is unavailable; install git to sync your notes."))?;
            let start = Instant::now();
            let status = loop {
                if let Some(status) = child
                    .try_wait()
                    .map_err(|_| invalid("Sync could not finish; it will retry."))?
                {
                    break status;
                }
                if start.elapsed() >= transport_timeout() {
                    #[cfg(windows)]
                    {
                        let _ = job.terminate(1);
                    }
                    #[cfg(unix)]
                    {
                        let _ = rustix::process::kill_process_group(
                            rustix::process::Pid::from_child(&child),
                            rustix::process::Signal::KILL,
                        );
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(invalid(
                        "Sync timed out; your notes are saved and it will retry.",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            };
            // The leader exited: end any descendant (ssh, credential
            // helpers) before reading the spooled output.
            #[cfg(windows)]
            drop(job);
            let bytes = std::fs::read(&out_path).map_err(|e| StoreError::io(&out_path, e))?;
            Ok((status.success(), bytes, status.code().unwrap_or(-1)))
        })();
        let _ = std::fs::remove_file(out_path);
        result
    }
}

fn transport_timeout() -> Duration {
    if cfg!(feature = "fault-injection")
        && let Some(ms) = std::env::var("WI_SYNC_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
    {
        return Duration::from_millis(ms.clamp(1, 60_000));
    }
    TIMEOUT
}
