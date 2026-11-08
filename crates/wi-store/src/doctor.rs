//! `wi doctor` checks: git state, manifest health, index health and
//! fallback, watcher, cloud-sync folder, pending work.

use serde::Serialize;
use wi_history::{History, Manifest, PublisherLock, cloud_sync_warning};

use crate::error::Result;
use crate::paths::{CONFLICT_MARK, MOVED_MARK, TMP_MARK};
use crate::vault::Vault;
use crate::work::WorkKind;

/// One check.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    /// `ok`, `warn` or `fail`.
    pub status: &'static str,
    pub detail: String,
    /// What to do next, when not ok.
    pub fix: Option<String>,
}

fn check(name: &'static str, status: &'static str, detail: String, fix: Option<&str>) -> Check {
    Check {
        name,
        status,
        detail,
        fix: fix.map(str::to_owned),
    }
}

/// All checks.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub root: String,
    pub vault_id: String,
    pub generation: u64,
    pub checks: Vec<Check>,
}

impl DoctorReport {
    pub fn healthy(&self) -> bool {
        self.checks.iter().all(|c| c.status != "fail")
    }
}

impl Vault {
    /// Run the basic health checks.
    pub fn doctor(&self) -> Result<DoctorReport> {
        let mut checks = Vec::new();
        checks.push(match History::head(&self.history) {
            Ok(Some(head)) => check("git", "ok", format!("branch tip {head}"), None),
            Ok(None) => check(
                "git",
                "warn",
                "no commits yet".into(),
                Some("run `wi commit`"),
            ),
            Err(e) => check(
                "git",
                "fail",
                e.to_string(),
                Some("check the repository with `git status`"),
            ),
        });
        let manifest = {
            let guard = PublisherLock::acquire(&self.history)?;
            Manifest::snapshot(&self.history, &guard)
        };
        checks.push(match manifest {
            Ok(snap) if snap.open_intent().is_some() => check(
                "manifest",
                "warn",
                format!(
                    "interrupted write(s) open on {}; only writes to these paths wait (recovery-held)",
                    snap.open_intents()
                        .map(|e| e.op.path())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Some("run any `wi` command; opening the vault settles it once the path is readable"),
            ),
            Ok(snap) if snap.entries.is_empty() => {
                check("manifest", "ok", "no pending records".into(), None)
            }
            Ok(snap) => check(
                "manifest",
                "warn",
                format!("{} record(s) not yet committed", snap.entries.len()),
                Some("run `wi commit`"),
            ),
            Err(e) => check(
                "manifest",
                "fail",
                e.to_string(),
                Some("run `wi doctor` again; opening replays and quarantines a corrupt tail"),
            ),
        });
        let pending = self.index_pending();
        let index = self.with_index(|index| Ok(index.index_as_of()?));
        // The index is a disposable cache: maintenance repairs or rebuilds
        // it by itself, so nothing here asks anyone to run a command.
        const SELF_REPAIR: &str = "repairs itself on the next maintenance pass";
        checks.push(match index {
            Ok(Some(as_of)) if pending.is_empty() => {
                check("index", "ok", format!("complete walk at {as_of}"), None)
            }
            Ok(Some(_)) => check(
                "index",
                "warn",
                format!("{} path(s) pending re-index; {SELF_REPAIR}", pending.len()),
                None,
            ),
            Ok(None) => check(
                "index",
                "warn",
                format!("no complete walk yet; search uses the literal fallback and {SELF_REPAIR}"),
                None,
            ),
            Err(e) => check(
                "index",
                "warn",
                format!("index unusable ({e}); search uses the literal fallback and {SELF_REPAIR}"),
                None,
            ),
        });
        checks.push(check(
            "watcher",
            "ok",
            "no watcher in this process; one-shot queries run a stat-walk catch-up, `wi watch` ingests editor changes".into(),
            None,
        ));
        checks.push(match cloud_sync_warning(self.history.git_dir()) {
            Some(warning) => check(
                "cloud-sync",
                "warn",
                warning,
                Some("move the notes folder out of the cloud-synced folder, or keep its history outside it (folder mode)"),
            ),
            None => check(
                "cloud-sync",
                "ok",
                "history is not in a known cloud-sync folder".into(),
                None,
            ),
        });
        checks.push(self.storage_check());
        checks.push(self.unseen_check());
        let work = self.work_requests()?;
        checks.push(if work.is_empty() {
            check("work", "ok", "no work requests".into(), None)
        } else {
            check(
                "work",
                "warn",
                format!("{} work request(s) recorded", work.len()),
                Some("maintenance turns them into work items for your agent"),
            )
        });
        let (shared, orphans): (Vec<String>, Vec<String>) =
            unannounced_conflict_copies(self.root(), &work)
                .into_iter()
                .partition(|rel| self.committed_sync_copy(rel));
        if !shared.is_empty() {
            // Sync copies are committed and reach every device; the work
            // request lives on the device that merged, and its work item
            // is listed read-only here once that device converts it.
            checks.push(check(
                "sync-conflict-copies",
                "ok",
                format!(
                    "{} sync conflict cop(ies) kept by another device (read-only here): {}",
                    shared.len(),
                    shared.join(", ")
                ),
                None,
            ));
        }
        if !orphans.is_empty() {
            checks.push(check(
                "conflict-copies",
                "warn",
                format!(
                    "{} conflict cop(ies) without a work request: {}",
                    orphans.len(),
                    orphans.join(", ")
                ),
                Some("review each copy against its note, then remove it"),
            ));
        }
        let temps: Vec<String> = scratch_temps(self.root())
            .into_iter()
            .map(|(rel, _)| rel)
            .collect();
        if !temps.is_empty() {
            checks.push(check(
                "temp-files",
                "warn",
                format!("{} engine temp file(s): {}", temps.len(), temps.join(", ")),
                Some("maintenance removes each one once the program that wrote it has stopped"),
            ));
        }
        let asides = moved_asides(self.root());
        if !asides.is_empty() {
            checks.push(check(
                "moved-sources",
                "warn",
                format!(
                    "{} move source(s) left aside by an interrupted move: {}",
                    asides.len(),
                    asides.join(", ")
                ),
                Some(
                    "run any `wi` command to settle an open move; otherwise review and remove them",
                ),
            ));
        }
        let large = large_attachments(self.root());
        if !large.is_empty() {
            checks.push(check(
                "large-attachments",
                "warn",
                format!(
                    "{} attachment(s) exceed the unmeasured 20 MiB policy threshold: {}",
                    large.len(),
                    large.join(", ")
                ),
                Some("consider Git LFS; no automatic LFS configuration is performed"),
            ));
        }
        Ok(DoctorReport {
            root: self.root().display().to_string(),
            vault_id: self.vault_id.clone(),
            generation: self.generation()?,
            checks,
        })
    }
}

impl Vault {
    /// `storage`: where history lives, whether a moved folder was re-bound,
    /// and which device runs lifecycle maintenance in folder mode.
    fn storage_check(&self) -> Check {
        let mut detail = match self.storage_mode() {
            wi_history::StorageMode::Git => {
                "git mode: history in .git inside the notes folder".to_owned()
            }
            wi_history::StorageMode::Folder => format!(
                "folder mode: history in {} on this device",
                self.history.git_dir().display()
            ),
        };
        if let Some(from) = self.history.rebound_from() {
            detail.push_str(&format!(
                "; the notes folder moved from {} and history followed it",
                from.display()
            ));
        }
        if self.storage_mode() == wi_history::StorageMode::Folder {
            detail.push_str(if self.is_primary_device() {
                "; this device runs lifecycle maintenance"
            } else {
                "; another device runs lifecycle maintenance (`wi setup --primary` takes over)"
            });
        }
        check("storage", "ok", detail, None)
    }

    /// `unseen`: eligible files whose bytes history does not know (full
    /// compare, read-only).
    fn unseen_check(&self) -> Check {
        match self.full_survey().map(|s| (s.unseen, s.unreadable)) {
            Ok((_, unreadable)) if !unreadable.is_empty() => {
                let first: Vec<&str> = unreadable
                    .iter()
                    .take(UNREADABLE_SHOWN)
                    .map(|(p, _)| p.as_str())
                    .collect();
                check(
                    "unseen",
                    "warn",
                    format!(
                        "{} file(s) could not be read to compare with history: {}{}",
                        unreadable.len(),
                        first.join(", "),
                        if unreadable.len() > first.len() {
                            ", ..."
                        } else {
                            ""
                        }
                    ),
                    Some("check the file permissions, then run any wi command or `wi commit`"),
                )
            }
            Ok((paths, _)) if paths.is_empty() => check(
                "unseen",
                "ok",
                "every file is recorded in history".into(),
                None,
            ),
            Ok((paths, _)) => check(
                "unseen",
                "warn",
                format!(
                    "{} file(s) changed outside the engine are not in history yet",
                    paths.len()
                ),
                Some(UNSEEN_FIX),
            ),
            Err(e) => check(
                "unseen",
                "warn",
                format!("could not compare files with history: {e}"),
                Some(UNSEEN_FIX),
            ),
        }
    }
}

/// How many unreadable paths `unseen` names.
const UNREADABLE_SHOWN: usize = 5;

const UNSEEN_FIX: &str = "run any wi command, `wi commit`, or install `wi service`";

/// Engine temp files under `root` with the pid that created them.
pub(crate) fn scratch_temps(root: &std::path::Path) -> Vec<(String, Option<i32>)> {
    scratch_files(root, TMP_MARK)
}

/// Move sources a write took aside (`.<name>.wi-moved-<seq>`). Opening the
/// vault settles the one of an interrupted move; any other is stranded.
pub(crate) fn moved_asides(root: &std::path::Path) -> Vec<String> {
    scratch_files(root, MOVED_MARK)
        .into_iter()
        .map(|(rel, _)| rel)
        .collect()
}

impl Vault {
    /// Whether `rel` is a sync conflict copy (`.<name>.wi-conflict-sync-<12
    /// hex>`) committed at the branch tip: sync wrote it on some device.
    fn committed_sync_copy(&self, rel: &str) -> bool {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let is_sync = name
            .rsplit_once(".wi-conflict-sync-")
            .is_some_and(|(_, hex)| {
                hex.len() == 12 && hex.bytes().all(|b| b.is_ascii_hexdigit())
            });
        is_sync
            && History::head(&self.history)
                .ok()
                .flatten()
                .and_then(|head| History::blob_at(&self.history, &head, rel).ok().flatten())
                .is_some()
    }
}

/// Conflict copies (`.<name>.wi-conflict-<seq>[-<n>]`) that no
/// `conflict-copy` work request names: a writer stopped between keeping
/// the bytes and recording the request, outside an open intent.
fn unannounced_conflict_copies(
    root: &std::path::Path,
    work: &[crate::work::WorkRequest],
) -> Vec<String> {
    scratch_files(root, CONFLICT_MARK)
        .into_iter()
        .filter(|(rel, seq)| {
            let (dir, name) = match rel.rsplit_once('/') {
                Some((dir, name)) => (Some(dir), name),
                None => (None, rel.as_str()),
            };
            let original = name
                .strip_prefix('.')
                .and_then(|n| n.split_once(CONFLICT_MARK))
                .map(|(n, _)| match dir {
                    Some(dir) => format!("{dir}/{n}"),
                    None => n.to_owned(),
                });
            !work.iter().any(|w| {
                w.kind == WorkKind::ConflictCopy
                    && (w.detail.contains(rel.as_str())
                        || (Some(&w.path) == original.as_ref()
                            && w.seq.is_some()
                            && w.seq.and_then(|s| i32::try_from(s).ok()) == *seq))
            })
        })
        .map(|(rel, _)| rel)
        .collect()
}

/// Entries under `root` named `.<name><mark><rest>`, with the number that
/// starts `<rest>` (a pid for temp files, a sequence id for asides).
fn scratch_files(root: &std::path::Path, mark: &str) -> Vec<(String, Option<i32>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" || (dir == root && is_foreign_dir(&name)) {
                continue;
            }
            if let Some((_, rest)) = name.split_once(mark)
                && name.starts_with('.')
                && let Ok(rel) = entry.path().strip_prefix(root)
            {
                let pid = rest.split('-').next().and_then(|p| p.parse().ok());
                out.push((rel.display().to_string(), pid));
            } else if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

/// Whether the process that created a temp file is gone.
#[cfg(unix)]
pub(crate) fn process_gone(pid: Option<i32>) -> bool {
    let Some(pid) = pid.and_then(rustix::process::Pid::from_raw) else {
        return false;
    };
    matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

/// Without a process-liveness probe, never delete another writer's temp file.
#[cfg(not(unix))]
pub(crate) fn process_gone(_pid: Option<i32>) -> bool {
    false
}

/// Another application's top-level folder (never inspected).
fn is_foreign_dir(name: &str) -> bool {
    crate::paths::FOREIGN_DIRS.contains(&name.to_lowercase().as_str())
}

/// Unmeasured policy threshold: advise LFS for attachments over 20 MiB.
fn large_attachments(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let top = dir == root;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == ".git"
                || name == ".wiki-island"
                || (top && is_foreign_dir(&name.to_string_lossy()))
            {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file()
                && entry
                    .path()
                    .extension()
                    .is_none_or(|ext| !ext.eq_ignore_ascii_case("md"))
                && entry.metadata().is_ok_and(|m| m.len() > 20 * 1024 * 1024)
                && let Ok(rel) = entry.path().strip_prefix(root)
            {
                out.push(rel.display().to_string());
            }
        }
    }
    out.sort();
    out
}
