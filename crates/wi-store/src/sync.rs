//! Explicit remote transport and crash-safe local application. Capture,
//! reads, maintenance and MCP never call this module's transport methods.
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use wi_history::{BlobId, PublisherLock, SyncRepo, SyncTree, hash_bytes};

use crate::error::{Result, StoreError};
use crate::vault::Vault;
use crate::work::{WorkKind, WorkRequest};

mod background;
mod exclusions;
mod journal;
mod transport;

pub use background::SyncHandle;

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
            // A name this device cannot store (Windows D7) is not a protected
            // path: it stays in history and is skipped here as uncreatable.
            let normalized = if crate::paths::host_can_name(path) {
                crate::paths::normalize(self.root(), path)?
            } else {
                path.clone()
            };
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
        if !crate::paths::host_can_name(path) {
            return false;
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
