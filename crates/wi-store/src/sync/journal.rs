//! Applying a remote tree and the crash-recovery journal.

use std::collections::BTreeSet;

use wi_history::{FlushLock, Manifest, PublisherLock, SyncRepo, durable, hash_bytes};

use crate::contained::{Dir, read_rel};
use crate::error::{Result, StoreError};
use crate::exchange::{place, rename_no_replace};
use crate::vault::Vault;
use crate::work::{WorkKind, WorkRequest};

use super::*;

impl Vault {
    pub(super) fn apply_remote(
        &self,
        remote: Option<&str>,
        excluded: Vec<String>,
    ) -> Result<SyncReport> {
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
        #[cfg(any(unix, windows))]
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
                        .and_then(|(p, _)| p.parse::<i32>().ok());
                    let Some(owner) = owner else {
                        continue;
                    };
                    if owner_gone(owner) {
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
    pub(super) fn save_kept_edits(&self, kept: &[String]) -> Result<()> {
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
}

/// Whether the transport owner `pid` has exited (no such process).
#[cfg(unix)]
fn owner_gone(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid).is_some_and(|owner| {
        rustix::process::test_kill_process(owner) == Err(rustix::io::Errno::SRCH)
    })
}

/// Whether the transport owner `pid` has exited (D13: unknown is alive).
#[cfg(windows)]
fn owner_gone(pid: i32) -> bool {
    crate::doctor::process_gone(Some(pid))
}
