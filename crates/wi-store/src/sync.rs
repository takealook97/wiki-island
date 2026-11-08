//! Explicit remote transport and crash-safe local application. Capture,
//! reads, maintenance and MCP never call this module's transport methods.
use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wi_history::{
    BlobId, FlushLock, Manifest, PublisherLock, SyncRepo, SyncTree, durable, hash_bytes,
};

use crate::contained::{Dir, read_rel};
use crate::error::{Result, StoreError};
use crate::exchange::{place, rename_no_replace};
use crate::vault::Vault;
use crate::work::{WorkKind, WorkRequest};

const CONFIG: &str = "sync-config.json";
const STATE: &str = "sync-state.json";
const INTENT: &str = "sync-intent.json";
/// Files above this size never enter history (capture, writes, ingest).
pub(crate) const LIMIT: u64 = 50 * 1024 * 1024;
/// The comment line above each managed large-file exclude rule.
const LARGE_MARK: &str = "# wiki-island: file exceeds 50 MB";
const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    url: String,
    branch: String,
}

/// Persisted retry state, private to this device.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncStatus {
    pub configured: bool,
    pub failures: u32,
    pub failing_since_ms: Option<u64>,
    pub next_attempt_ms: u64,
    pub last_success_ms: Option<u64>,
    pub health_reported: bool,
    /// One plain line, once after 24 hours of continuous failure.
    pub health: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncReport {
    pub changed: Vec<String>,
    pub conflicts: Vec<String>,
    pub excluded: Vec<String>,
    pub pushed: bool,
    pub deferred: bool,
    /// Notes edited on this device while sync was applying: the edit stays
    /// visible and is saved as a follow-up local change.
    pub kept_local: Vec<String>,
    pub status: SyncStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Update {
    path: String,
    old: Option<BlobId>,
    new: Option<BlobId>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Intent {
    actor: crate::Ingress,
    seq: u64,
    expected: Option<String>,
    target: String,
    updates: Vec<Update>,
    conflicts: Vec<(String, String)>,
}

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}

impl Vault {
    fn sync_file(&self, name: &str) -> PathBuf {
        self.history.wi_dir().join(name)
    }

    fn load_sync<T: serde::de::DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let path = self.sync_file(name);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| invalid("The saved sync state cannot be read; your notes are safe.")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::io(path, e)),
        }
    }
    fn save_sync(&self, name: &str, value: &impl Serialize) -> Result<()> {
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
    fn transport(&self, args: &[&str]) -> Result<(bool, Vec<u8>, i32)> {
        let id = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
        let out_path = self.sync_file(&format!("transport-{id}.out"));
        let result = (|| {
            let out = File::create(&out_path).map_err(|e| StoreError::io(&out_path, e))?;
            let mut command = Command::new("git");
            command
                .arg("--git-dir")
                .arg(self.history.git_dir())
                .arg("--work-tree")
                .arg(self.root())
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "protocol.ext.allow=never",
                    "-c",
                    "credential.interactive=false",
                ])
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
                .env("GIT_ASKPASS", "/usr/bin/false")
                .env("SSH_ASKPASS", "/usr/bin/false")
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
            let mut child = command
                .spawn()
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
            let bytes = std::fs::read(&out_path).map_err(|e| StoreError::io(&out_path, e))?;
            Ok((status.success(), bytes, status.code().unwrap_or(-1)))
        })();
        let _ = std::fs::remove_file(out_path);
        result
    }

    /// One explicit attempt ignores the scheduler backoff. Transport is
    /// outside every engine lock; a fresh local snapshot is taken afterward.
    pub fn sync(&self) -> Result<SyncReport> {
        self.sync_attempt(false)
    }

    /// Background attempt honors the device's persisted backoff.
    pub fn sync_background(&self) -> Result<SyncReport> {
        self.sync_attempt(true)
    }

    fn sync_attempt(&self, background: bool) -> Result<SyncReport> {
        let config = self
            .load_sync::<Config>(CONFIG)?
            .ok_or_else(|| invalid("Sync is not connected; run wi sync setup <url>."))?;
        if self.storage_mode() != wi_history::StorageMode::Git {
            return Err(invalid("Git remote sync is available in git mode only."));
        }
        let status = self.sync_status()?;
        let now = crate::work::now_ms();
        if background && now < status.next_attempt_ms {
            return Ok(SyncReport {
                deferred: true,
                status,
                ..Default::default()
            });
        }
        let result = self.sync_inner(&config);
        let now = crate::work::now_ms();
        let guard = PublisherLock::acquire(&self.history)?;
        let mut status = self.sync_status()?;
        status.health = None;
        match &result {
            Ok(_) => {
                status.failures = 0;
                status.failing_since_ms = None;
                status.next_attempt_ms = 0;
                status.last_success_ms = Some(now);
                status.health_reported = false;
            }
            Err(_) => {
                status.failures = status.failures.saturating_add(1);
                let first = *status.failing_since_ms.get_or_insert(now);
                let seconds = 60u64
                    .saturating_mul(1u64 << status.failures.saturating_sub(1).min(6))
                    .min(3600);
                status.next_attempt_ms = now.saturating_add(seconds * 1000);
                if now.saturating_sub(first) >= 24 * 60 * 60 * 1000 && !status.health_reported {
                    status.health = Some("Notes are saved here, but sync has been unavailable for a day; check your connection and remote access.".into());
                    status.health_reported = true;
                }
            }
        }
        self.save_sync(STATE, &status)?;
        drop(guard);
        result.map(|mut report| {
            report.status = status;
            report
        })
    }

    fn sync_inner(&self, config: &Config) -> Result<SyncReport> {
        self.recover_sync()?;
        let excluded = self.exclude_large_files()?;
        let ingress = self.auto_ingest()?;
        if !ingress.failed.is_empty() {
            return Err(invalid(
                "Some notes are waiting to be saved; sync will retry.",
            ));
        }
        let flushed = self.flush()?;
        if flushed.remaining != 0 {
            return Err(invalid("Some saves are still finishing; sync will retry."));
        }
        crate::fault::abort_at("sync_after_flush");
        let (head_ok, remote_head, _) =
            self.transport(&["ls-remote", "--symref", &config.url, "HEAD"])?;
        if !head_ok {
            return Err(invalid(
                "Sync could not reach the remote; check your connection and remote access.",
            ));
        }
        let remote_head = String::from_utf8_lossy(&remote_head);
        let branch = remote_head
            .lines()
            .find_map(|line| {
                line.strip_prefix("ref: ")
                    .and_then(|s| s.strip_suffix("\tHEAD"))
            })
            .map(str::to_owned)
            .unwrap_or_else(|| empty_remote_branch(config));
        let (ok, refs, code) =
            self.transport(&["ls-remote", "--exit-code", &config.url, &branch])?;
        if !ok && code != 2 {
            return Err(invalid(
                "Sync could not reach the remote; check your connection and remote access.",
            ));
        }
        let fetch_ref = format!(
            "refs/wi-sync/{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        let remote = if refs.is_empty() {
            None
        } else {
            let spec = format!("{branch}:{fetch_ref}");
            let (ok, _, _) = self.transport(&[
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-auto-maintenance",
                "--no-recurse-submodules",
                &config.url,
                &spec,
            ])?;
            if !ok {
                return Err(invalid("Sync could not download notes; it will retry."));
            }
            let text = std::fs::read_to_string(self.history.git_dir().join(&fetch_ref))
                .map_err(|_| invalid("Downloaded notes could not be read; sync will retry."))?;
            Some(text.trim().to_owned())
        };
        crate::fault::abort_at("sync_after_fetch");
        let applied = self.apply_remote(remote.as_deref(), excluded);
        // Private temporary ref only; never a branch or a user's ref.
        let _ = std::fs::remove_file(self.history.git_dir().join(&fetch_ref));
        let mut report = applied?;
        if !report.kept_local.is_empty() {
            self.save_kept_edits(&report.kept_local)?;
            let flushed = self.flush()?;
            if flushed.remaining != 0 {
                return Err(invalid("Some saves are still finishing; sync will retry."));
            }
        }
        crate::fault::abort_at("sync_before_push");
        if let Some(head) = SyncRepo::head(&self.history)? {
            let large =
                SyncRepo::outgoing_large_paths(&self.history, &head, remote.as_deref(), LIMIT)?;
            if !large.is_empty() {
                for path in large {
                    self.record_work(WorkRequest::new(WorkKind::RecoveryProblem, path, "A file larger than 50 MB is already in local history; notes are saved here and upload was stopped without rewriting history."))?;
                }
                return Err(invalid(
                    "Local history contains a file larger than 50 MB; your notes are saved here and upload was stopped.",
                ));
            }
            let spec = format!("{head}:{branch}");
            let (ok, _, _) =
                self.transport(&["push", "--porcelain", "--no-verify", &config.url, &spec])?;
            if !ok {
                return Err(invalid(
                    "Sync could not upload notes; they are saved here and it will retry.",
                ));
            }
            report.pushed = true;
        }
        crate::fault::abort_at("sync_after_push");
        Ok(report)
    }

    /// Refuse protected or colliding paths. Returns the paths this device's
    /// file system cannot create (see [`Vault::creatable`]): they stay in
    /// history and on other devices, and sync goes on without them here.
    fn validate_sync_tree(&self, tree: &SyncTree) -> Result<BTreeSet<String>> {
        let mut identities = BTreeSet::new();
        for path in tree.keys() {
            let normalized = crate::paths::normalize(self.root(), path)?;
            if normalized != *path
                || sync_internal(path)
                || path
                    .split('/')
                    .any(|part| crate::paths::fold(part) == ".git")
            {
                return Err(invalid(
                    "The remote contains a protected path; notes were left unchanged.",
                ));
            }
            if !identities.insert(crate::paths::fold(path)) {
                return Err(invalid(
                    "The remote contains colliding file names; notes were left unchanged.",
                ));
            }
        }
        for path in &identities {
            for (at, _) in path.match_indices('/') {
                if identities.contains(&path[..at]) {
                    return Err(invalid(
                        "The remote has a file where a folder is needed; notes were left unchanged.",
                    ));
                }
            }
        }
        Ok(tree
            .keys()
            .filter(|path| !self.creatable(path))
            .cloned()
            .collect())
    }

    /// Whether `path` fits this device's file system under the vault root,
    /// with room for the sibling staging and conflict-copy names sync uses:
    /// every folder name at most 255 bytes, the file name at most 224, the
    /// whole path under the platform path limit, and no name the file
    /// system rejects outright.
    pub(crate) fn creatable(&self, path: &str) -> bool {
        const NAME_MAX: usize = 255;
        // `.<name>.wi-conflict-sync-<12 hex>` is the longest sibling.
        const SIBLING: usize = 31;
        const PATH_MAX: usize = if cfg!(target_os = "macos") {
            1024
        } else {
            4096
        };
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            let limit = if parts.peek().is_some() {
                NAME_MAX
            } else {
                NAME_MAX - SIBLING
            };
            if part.is_empty() || part.len() > limit {
                return false;
            }
        }
        let full = self.root().join(path);
        if full.as_os_str().len() + SIBLING >= PATH_MAX {
            return false;
        }
        !matches!(
            std::fs::symlink_metadata(&full),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidFilename
        )
    }

    fn apply_remote(&self, remote: Option<&str>, excluded: Vec<String>) -> Result<SyncReport> {
        let _flush = FlushLock::acquire(&self.history)?;
        let guard = PublisherLock::acquire(&self.history)?;
        if self.sync_file(INTENT).exists() {
            return Err(invalid("Sync recovery is still finishing; it will retry."));
        }
        let snap = Manifest::snapshot(&self.history, &guard)?;
        if !snap.entries.is_empty() {
            return Err(invalid("Notes changed during download; sync will retry."));
        }
        let local = SyncRepo::head(&self.history)?;
        let before = SyncRepo::tree(&self.history, local.as_deref())?;
        let mut after = before.clone();
        let mut conflicts = Vec::new();
        let mut target = local.clone();
        if let Some(remote) = remote {
            let theirs = SyncRepo::tree(&self.history, Some(remote))?;
            let unwritable = self.validate_sync_tree(&theirs)?;
            for path in &unwritable {
                if before.get(path) != theirs.get(path) {
                    crate::work::record(
                        &self.history,
                        &WorkRequest::new(
                            WorkKind::RecoveryProblem,
                            path,
                            "This device cannot create this file name (too long or not allowed here); it stays in history and on other devices, and sync continues without it.",
                        ),
                    )?;
                }
            }
            if let Some(blob) = theirs.get(wi_note::VAULT_CONFIG_PATH) {
                let bytes = self.history.read_blob(blob)?;
                let text = std::str::from_utf8(&bytes).map_err(|_| invalid("The remote notes configuration is unreadable; local notes were left unchanged."))?;
                let config = wi_note::VaultConfig::parse(text)?;
                if config.vault_id().is_some_and(|id| id != self.vault_id()) {
                    return Err(invalid(
                        "The remote belongs to another notes folder; connect a clone of that folder instead.",
                    ));
                }
            }
            for blob in theirs.values() {
                if SyncRepo::blob_size(&self.history, blob)? > LIMIT {
                    return Err(invalid(
                        "The remote contains a file larger than 50 MB; local notes were left unchanged.",
                    ));
                }
            }
            let base = if local.as_deref() == Some(remote) {
                local.clone()
            } else {
                local
                    .as_deref()
                    .map(|l| SyncRepo::merge_base(&self.history, l, remote))
                    .transpose()?
                    .flatten()
            };
            if local.as_deref() == Some(remote) || base.as_deref() == Some(remote) { /* remote is already local history */
            } else if local.is_none() || base == local {
                after = theirs;
                target = Some(remote.into());
            } else {
                let ancestor = SyncRepo::tree(&self.history, base.as_deref())?;
                let paths: BTreeSet<_> = before
                    .keys()
                    .chain(theirs.keys())
                    .chain(ancestor.keys())
                    .cloned()
                    .collect();
                for path in paths {
                    let ours = before.get(&path);
                    let theirs = theirs.get(&path);
                    let old = ancestor.get(&path);
                    if ours == theirs || theirs == old {
                        continue;
                    }
                    if ours == old {
                        match theirs {
                            Some(blob) => {
                                after.insert(path.clone(), blob.clone());
                            }
                            None => {
                                after.remove(&path);
                            }
                        }
                    } else {
                        let copy = conflict_path(&path, remote);
                        if let Some(blob) = theirs {
                            if after.get(&copy).is_some_and(|existing| existing != blob) {
                                return Err(invalid(
                                    "A sync conflict copy already has other bytes; notes were left unchanged.",
                                ));
                            }
                            after.insert(copy.clone(), blob.clone());
                        }
                        conflicts.push((
                            path,
                            if theirs.is_some() {
                                copy
                            } else {
                                "remote deletion; local version kept".into()
                            },
                        ));
                    }
                }
                self.validate_sync_tree(&after)?;
                target = Some(SyncRepo::merge(
                    &self.history,
                    &guard,
                    local
                        .as_deref()
                        .ok_or_else(|| invalid("Sync needs local history."))?,
                    remote,
                    &after,
                )?);
            }
        }
        // A tracked file that grew past the limit keeps its last committed
        // version in the synchronized tree: the new bytes stay local and
        // out of history, and no device ever sees the note deleted.
        self.validate_sync_tree(&after)?;
        if let Some(blob) = after.get(wi_note::VAULT_CONFIG_PATH) {
            let bytes = self.history.read_blob(blob)?;
            let text = std::str::from_utf8(&bytes).map_err(|_| invalid("The remote notes configuration is unreadable; local notes were left unchanged."))?;
            let config = wi_note::VaultConfig::parse(text)?;
            if config.vault_id() != Some(self.vault_id()) {
                return Err(invalid(
                    "The remote belongs to another notes folder; local notes were left unchanged.",
                ));
            }
        } else {
            return Err(invalid(
                "The remote removed the notes configuration; local notes were left unchanged.",
            ));
        }
        let Some(target) = target else {
            return Ok(SyncReport {
                excluded,
                ..Default::default()
            });
        };
        let mut updates = Vec::new();
        for path in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
            if before.get(path) == after.get(path)
                || excluded.contains(path)
                || !self.creatable(path)
            {
                continue;
            }
            let old = before.get(path).cloned();
            let new = after.get(path).cloned();
            let current = read_rel(self.root(), path)?
                .map(|b| wi_history::blob_id_of(&b))
                .transpose()?;
            if current != old {
                return Err(invalid("A note changed during download; sync will retry."));
            }
            updates.push(Update {
                path: path.clone(),
                old,
                new,
            });
        }
        if local.as_deref() == Some(&target) {
            return Ok(SyncReport {
                excluded,
                ..Default::default()
            });
        }
        SyncRepo::barrier_objects(&self.history)?;
        let intent = Intent {
            actor: crate::Ingress::Sync,
            seq: snap.next_seq,
            expected: local,
            target,
            updates,
            conflicts,
        };
        self.save_sync(INTENT, &intent)?;
        crate::fault::abort_at("sync_after_intent");
        let kept_local = self.replay_sync_locked(&guard, &intent)?;
        Ok(SyncReport {
            changed: intent
                .updates
                .iter()
                .map(|u| u.path.clone())
                .filter(|p| !kept_local.contains(p))
                .collect(),
            kept_local,
            conflicts: intent
                .conflicts
                .iter()
                .map(|(_, copy)| copy.clone())
                .collect(),
            excluded,
            ..Default::default()
        })
    }

    /// Retire transport scratch from dead owners. Live attempts keep
    /// their own temporary refs until application finishes.
    pub(crate) fn cleanup_sync_scratch(&self) -> Result<()> {
        #[cfg(unix)]
        {
            let _guard = PublisherLock::acquire(&self.history)?;
            for (dir, prefix) in [
                (self.history.wi_dir(), "transport-"),
                (self.history.git_dir().join("refs/wi-sync"), ""),
            ] {
                if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                    continue;
                }
                let entries = std::fs::read_dir(&dir).map_err(|e| StoreError::io(&dir, e))?;
                let mut removed = 0;
                for entry in entries.flatten() {
                    if removed >= 256 {
                        break;
                    }
                    if !entry.file_type().is_ok_and(|t| t.is_file()) {
                        continue;
                    }
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    let owner = name
                        .strip_prefix(prefix)
                        .and_then(|s| s.split_once('-'))
                        .and_then(|(p, _)| p.parse::<i32>().ok())
                        .and_then(rustix::process::Pid::from_raw);
                    let Some(owner) = owner else {
                        continue;
                    };
                    if rustix::process::test_kill_process(owner) == Err(rustix::io::Errno::SRCH) {
                        match std::fs::remove_file(entry.path()) {
                            Ok(()) => removed += 1,
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => return Err(StoreError::io(entry.path(), e)),
                        }
                    }
                }
                if removed > 0 {
                    durable::barrier_dir(&dir, self.history.durability())?;
                }
            }
        }
        Ok(())
    }

    /// Replay before ordinary recovery or any new publisher can proceed.
    pub(crate) fn recover_sync(&self) -> Result<()> {
        if !self.sync_file(INTENT).exists() {
            return Ok(());
        }
        let kept = {
            let _flush = FlushLock::acquire(&self.history)?;
            let guard = PublisherLock::acquire(&self.history)?;
            match self.load_sync::<Intent>(INTENT)? {
                Some(intent) => self.replay_sync_locked(&guard, &intent)?,
                None => Vec::new(),
            }
        };
        self.save_kept_edits(&kept)
    }

    /// Save edits that sync left in place as ordinary local changes. Runs
    /// without engine locks; the intent is already retired.
    fn save_kept_edits(&self, kept: &[String]) -> Result<()> {
        for path in kept {
            self.ingest_path(path)?;
        }
        Ok(())
    }

    /// Apply a durable sync intent. Returns the paths whose working file
    /// no longer held the expected old bytes: they were left in place.
    fn replay_sync_locked(
        &self,
        guard: &wi_history::PublisherGuard,
        intent: &Intent,
    ) -> Result<Vec<String>> {
        if intent.actor != crate::Ingress::Sync {
            return Err(invalid("Sync recovery refused an unknown ingress."));
        }
        let after = SyncRepo::tree(&self.history, Some(&intent.target))?;
        self.validate_sync_tree(&after)?;
        let mut seen = BTreeSet::new();
        for update in &intent.updates {
            if crate::paths::normalize(self.root(), &update.path)? != update.path
                || sync_internal(&update.path)
                || update
                    .path
                    .split('/')
                    .any(|part| crate::paths::fold(part) == ".git")
                || after.get(&update.path) != update.new.as_ref()
                || !seen.insert(&update.path)
            {
                return Err(invalid(
                    "Sync recovery refused an invalid file decision; notes were kept.",
                ));
            }
        }
        let head = SyncRepo::head(&self.history)?;
        if head != intent.expected && head.as_deref() != Some(&intent.target) {
            // Another git tool moved the branch after preparation. Retain
            // the prepared version under its own immutable ref; never
            // force the owner's branch back or require manual repair.
            SyncRepo::retain(&self.history, guard, &intent.target)?;
            for update in &intent.updates {
                if let Some((dir, name)) = Dir::open_parent(self.root(), &update.path)? {
                    let temp = format!(".{name}.wi-tmp-sync-{}", &intent.target[..12]);
                    self.settle_sync_temp(&dir, &temp, &name, update)?;
                }
                crate::work::record(
                    &self.history,
                    &WorkRequest::new(
                        WorkKind::RecoveryConflict,
                        &update.path,
                        "Another local history change interrupted sync; both versions are saved for review.",
                    ),
                )?;
            }
            Manifest::publish_sync(&self.history, guard, intent.seq)?;
            let paths: Vec<_> = intent.updates.iter().map(|u| u.path.as_str()).collect();
            self.index_update(&paths);
            self.retire_sync_intent()?;
            return Ok(Vec::new());
        }
        let mut kept = Vec::new();
        for update in &intent.updates {
            crate::paths::normalize(self.root(), &update.path)?;
            if sync_internal(&update.path) {
                return Err(invalid("Sync recovery refused a protected path."));
            }
            if !self.apply_sync_update(update, &intent.target)? {
                kept.push(update.path.clone());
            }
            crate::fault::abort_at("sync_after_file");
        }
        for (path, copy) in &intent.conflicts {
            // This helper takes the publisher lock itself; append directly
            // under our existing guard to avoid recursive locking.
            crate::work::record(
                &self.history,
                &WorkRequest::new(
                    WorkKind::ConflictCopy,
                    path,
                    format!("remote version preserved: {copy}"),
                ),
            )?;
        }
        crate::fault::abort_at("sync_before_ref");
        SyncRepo::advance(
            &self.history,
            guard,
            intent.expected.as_deref(),
            &intent.target,
        )?;
        crate::fault::abort_at("sync_after_ref");
        Manifest::publish_sync(&self.history, guard, intent.seq)?;
        crate::fault::abort_at("sync_after_generation");
        let paths: Vec<_> = intent.updates.iter().map(|u| u.path.as_str()).collect();
        self.index_update(&paths);
        crate::fault::abort_at("sync_after_index");
        self.retire_sync_intent()?;
        Ok(kept)
    }

    fn retire_sync_intent(&self) -> Result<()> {
        let path = self.sync_file(INTENT);
        std::fs::remove_file(&path).map_err(|e| StoreError::io(&path, e))?;
        durable::barrier_dir(&self.history.wi_dir(), self.history.durability())?;
        Ok(())
    }

    /// Bring one working file to the intent's new version. Returns false
    /// when the file holds neither the old nor the new version: an editor
    /// changed it after the intent was prepared, so it stays in place.
    fn apply_sync_update(&self, update: &Update, target: &str) -> Result<bool> {
        let mode = self.history.durability();
        let (dir, name) = Dir::create_parent(self.root(), &update.path, mode)?;
        let temp = format!(".{name}.wi-tmp-sync-{}", &target[..12]);
        let current = dir.read_regular(&name)?;
        let current_blob = current.as_deref().map(wi_history::blob_id_of).transpose()?;
        if current_blob != update.old && current_blob != update.new {
            // Only staging bytes this intent wrote are retired.
            self.settle_sync_temp(&dir, &temp, &name, update)?;
            return Ok(false);
        }
        // A deterministic staging name makes both the swap and its displaced
        // bytes recoverable after a process dies between any two syscalls.
        if current_blob != update.new {
            if let Some(new) = &update.new {
                if let Some(staged) = dir.read_regular(&temp)?
                    && wi_history::blob_id_of(&staged)? != *new
                {
                    self.settle_sync_temp(&dir, &temp, &name, update)?;
                }
                if !dir.exists(&temp)? {
                    dir.create_new(
                        &temp,
                        &self.history.read_blob(new)?,
                        mode,
                        dir.permissions(&name)?,
                    )?;
                    dir.barrier(mode)?;
                }
                let staged = dir
                    .read_regular(&temp)?
                    .ok_or_else(|| invalid("Sync staging bytes are missing."))?;
                if wi_history::blob_id_of(&staged)? != *new {
                    return Err(invalid(
                        "Sync staging contains unexpected bytes; notes were kept.",
                    ));
                }
                place(
                    &dir,
                    &temp,
                    &name,
                    current.as_deref().map(hash_bytes).as_deref(),
                )?;
                dir.barrier(mode)?;
            } else if dir.exists(&name)? {
                self.settle_sync_temp(&dir, &temp, &name, update)?;
                if !rename_no_replace(&dir, &name, &temp)? {
                    return Err(invalid("Sync deletion is awaiting recovery."));
                }
                dir.barrier(mode)?;
            }
        }
        self.settle_sync_temp(&dir, &temp, &name, update)?;
        Ok(true)
    }

    fn settle_sync_temp(&self, dir: &Dir, temp: &str, name: &str, update: &Update) -> Result<()> {
        let mode = self.history.durability();
        if let Some(displaced) = dir.read_regular(temp)? {
            let blob = wi_history::blob_id_of(&displaced)?;
            if Some(&blob) != update.old.as_ref() && Some(&blob) != update.new.as_ref() {
                let copy = copy_name(name, &hash_bytes(&displaced)[7..19]);
                crate::work::record(
                    &self.history,
                    &WorkRequest::new(
                        WorkKind::ConflictCopy,
                        &update.path,
                        format!("editor version preserved beside the note as {copy}"),
                    ),
                )?;
                if !rename_no_replace(dir, temp, &copy)? {
                    if dir.read_regular(&copy)?.as_deref() != Some(displaced.as_slice()) {
                        return Err(invalid(
                            "A recovery conflict copy already exists; notes were kept.",
                        ));
                    }
                    dir.remove(temp)?;
                }
            } else {
                dir.remove(temp)?;
            }
            dir.barrier(mode)?;
        }
        Ok(())
    }

    fn exclude_large_files(&self) -> Result<Vec<String>> {
        self.release_large_excludes(None)?;
        let mut large = Vec::new();
        for path in crate::ingest::walk_files(self.root()) {
            if std::fs::metadata(self.root().join(&path)).is_ok_and(|m| m.len() > LIMIT) {
                large.push(path);
            }
        }
        // Tracked files that grew past the limit are reported too; their
        // working bytes are never replaced by a remote version.
        if let Some(head) = SyncRepo::head(&self.history)? {
            for path in SyncRepo::tree(&self.history, Some(&head))?.keys() {
                if std::fs::metadata(self.root().join(path)).is_ok_and(|m| m.len() > LIMIT)
                    && !large.contains(path)
                {
                    large.push(path.clone());
                }
            }
        }
        if large.is_empty() {
            return Ok(large);
        }
        self.persist_large_excludes(&large)?;
        Ok(large)
    }

    /// Watcher and CLI ingress never commit a file larger than the limit,
    /// in either storage mode and with or without a remote: the file stays
    /// in the folder, gets a managed exclude rule and is reported once.
    pub(crate) fn sync_exclude_oversize(&self, rel: &str) -> Result<bool> {
        if !std::fs::metadata(self.root().join(rel)).is_ok_and(|m| m.len() > LIMIT) {
            return Ok(false);
        }
        self.persist_large_excludes(&[rel.to_owned()])?;
        Ok(true)
    }

    /// Drop the managed large-file rule of every path in `paths` (all
    /// managed rules when `None`) whose file is back at or under the
    /// limit, so a note that shrank is ingested and synced again. Returns
    /// the released paths.
    pub(crate) fn release_large_excludes(&self, paths: Option<&[&str]>) -> Result<Vec<String>> {
        let path = self.history.exclude_file();
        let shrunk = |text: &str| -> Vec<String> {
            let mut out = Vec::new();
            let mut lines = text.lines().peekable();
            while let Some(line) = lines.next() {
                if line != LARGE_MARK {
                    continue;
                }
                let Some(rule) = lines.peek().and_then(|r| r.strip_prefix('/')) else {
                    continue;
                };
                let rel = unescape_pattern(rule);
                let wanted = paths.is_none_or(|p| p.contains(&rel.as_str()));
                if wanted
                    && std::fs::metadata(self.root().join(&rel))
                        .is_ok_and(|m| m.is_file() && m.len() <= LIMIT)
                {
                    out.push(rel);
                }
            }
            out
        };
        // Cheap unlocked look first: most calls find nothing to release.
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if shrunk(&text).is_empty() {
            return Ok(Vec::new());
        }
        let _guard = PublisherLock::acquire(&self.history)?;
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let released = shrunk(&text);
        if released.is_empty() {
            return Ok(released);
        }
        let rules: Vec<String> = released
            .iter()
            .map(|rel| format!("/{}", escape_pattern(rel)))
            .collect();
        let mut kept = Vec::new();
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            if line == LARGE_MARK && lines.peek().is_some_and(|r| rules.iter().any(|x| x == r)) {
                lines.next();
                continue;
            }
            kept.push(line);
        }
        let mut out = kept.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        durable::write_atomic(&path, out.as_bytes(), self.history.durability())?;
        wi_index::set_exclude_file(self.root(), Some(&path));
        Ok(released)
    }

    pub(crate) fn persist_large_excludes(&self, large: &[String]) -> Result<()> {
        let _guard = PublisherLock::acquire(&self.history)?;
        let path = self.history.exclude_file();
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut reported = Vec::new();
        for rel in large {
            let rule = format!("/{}", escape_pattern(rel));
            if !text.lines().any(|l| l == rule) {
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(LARGE_MARK);
                text.push('\n');
                text.push_str(&rule);
                text.push('\n');
                reported.push(rel.clone());
            }
        }
        if reported.is_empty() {
            return Ok(());
        }
        // Record before excluding: a crash can repeat the deduplicated
        // request, but cannot hide a large file before its report is durable.
        for rel in &reported {
            crate::work::record(
                &self.history,
                &WorkRequest::new(
                    WorkKind::IngestFailed,
                    rel,
                    "This file is larger than 50 MB; it stays in the folder and is kept out of history and remote sync.",
                ),
            )?;
        }
        crate::fault::abort_at("sync_after_exclude_request");
        durable::write_atomic(&path, text.as_bytes(), self.history.durability())?;
        wi_index::set_exclude_file(self.root(), Some(&path));
        crate::fault::abort_at("sync_after_exclude");
        Ok(())
    }
}

// An unborn local bare remote has no advertised HEAD oid. Retain its
// symbolic branch so a subsequent ordinary clone checks out the notes.
fn empty_remote_branch(config: &Config) -> String {
    let local = config.url.strip_prefix("file://").unwrap_or(&config.url);
    let root = Path::new(local);
    if root.is_dir() {
        for head in [root.join("HEAD"), root.join(".git/HEAD")] {
            if let Ok(text) = std::fs::read_to_string(head)
                && let Some(branch) = text.trim().strip_prefix("ref: ")
                && branch.starts_with("refs/heads/")
                && !branch.contains(':')
                && !branch.chars().any(char::is_control)
            {
                return branch.into();
            }
        }
    }
    format!("refs/heads/{}", config.branch)
}

fn conflict_path(path: &str, remote: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/{}", copy_name(name, &remote[..12])),
        None => copy_name(path, &remote[..12]),
    }
}

/// `.<name>.wi-conflict-sync-<suffix>`. The copy must itself be creatable
/// (see `Vault::creatable`), so its file name stays within 224 bytes: a
/// long note name is shortened at a character boundary and ends in a
/// digest of the full name, keeping copies of different notes apart.
fn copy_name(name: &str, suffix: &str) -> String {
    const NAME_LIMIT: usize = 255 - 31;
    const ROOM: usize = NAME_LIMIT - ".".len() - ".wi-conflict-sync-".len() - 12;
    if name.len() <= ROOM {
        return format!(".{name}.wi-conflict-sync-{suffix}");
    }
    let digest = &hash_bytes(name.as_bytes())[7..15];
    let mut end = ROOM - 1 - digest.len();
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!(".{}~{digest}.wi-conflict-sync-{suffix}", &name[..end])
}

// Committed sync copies use the existing conflict-copy protocol. They are
// readable by path but never indexed or adopted as another active claim.
fn sync_internal(path: &str) -> bool {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let copy = name.starts_with('.')
        && name
            .rsplit_once(".wi-conflict-sync-")
            .is_some_and(|(_, suffix)| {
                suffix.len() == 12 && suffix.bytes().all(|c| c.is_ascii_hexdigit())
            });
    if copy {
        let probe = if parent.is_empty() {
            "sync-copy".into()
        } else {
            format!("{parent}/sync-copy")
        };
        crate::paths::is_engine_internal(&probe)
    } else {
        crate::paths::is_engine_internal(path)
    }
}

fn escape_pattern(path: &str) -> String {
    path.chars()
        .flat_map(|c| {
            if matches!(c, '\\' | '*' | '?' | '[' | ']' | '!' | '#' | ' ') {
                vec!['\\', c]
            } else {
                vec![c]
            }
        })
        .collect()
}

/// Separate background sync owner. The watcher and maintenance never wait
/// for transport; stopping joins this thread after its bounded attempt.
pub struct SyncHandle {
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl SyncHandle {
    pub fn stop(mut self) {
        self.finish();
    }
    fn finish(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for SyncHandle {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Vault {
    /// Start only for an explicitly configured git-mode vault. Runs at
    /// startup, 60 seconds after the latest observed publication, and at
    /// least every five minutes, subject to persisted transport backoff.
    pub fn start_sync(&self) -> Result<Option<SyncHandle>> {
        if self.storage_mode() != wi_history::StorageMode::Git || !self.sync_status()?.configured {
            return Ok(None);
        }
        let root = self.root().to_path_buf();
        let options = self.options.clone();
        let placement = self.placement.clone();
        let (stop, receive) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let Ok(vault) = Vault::open_placed(root, options, placement) else {
                return;
            };
            let mut last = Instant::now() - Duration::from_secs(300);
            let mut observed = None;
            let mut dirty = None;
            loop {
                let current = (vault.generation().ok(), SyncRepo::head(&vault.history).ok());
                if observed.as_ref() != Some(&current) {
                    observed = Some(current);
                    dirty = Some(Instant::now());
                }
                let now = Instant::now();
                if now.duration_since(last) >= Duration::from_secs(300)
                    || dirty.is_some_and(|at| now.duration_since(at) >= Duration::from_secs(60))
                {
                    match vault.sync_background() {
                        Ok(report) if !report.deferred => {
                            last = Instant::now();
                            dirty = None;
                        }
                        Ok(_) => {
                            last = Instant::now();
                        }
                        Err(_) => {
                            last = Instant::now();
                            if let Ok(status) = vault.sync_status()
                                && let Some(line) = status.health
                            {
                                eprintln!("{line}");
                            }
                        }
                    }
                }
                if !matches!(
                    receive.recv_timeout(Duration::from_secs(1)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
            }
        });
        Ok(Some(SyncHandle {
            stop,
            thread: Some(thread),
        }))
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

fn unescape_pattern(rule: &str) -> String {
    let mut out = String::new();
    let mut chars = rule.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            other => out.push(other),
        }
    }
    out
}
