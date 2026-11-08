//! File ingress: changes made outside the engine (an editor, a shell, a
//! sync tool). The watcher and `reconcile` publish an unmatched change as
//! an `ingest` record under the publisher lock (blob first, actor
//! `watcher`). A change whose bytes match the newest manifest record for
//! the path (or the committed blob when no record is pending) is the
//! engine's own write and is ignored; this is the watcher correlation of
//! OPERATIONS `#watcher`.

use std::path::Path;

use serde::Serialize;
use serde_json::json;
use wi_history::{
    Blob, BlobId, Deletion, History, IngestRecord, Manifest, ManifestSnapshot, Missing, OpStatus,
    PublisherLock, blob_id_of, hash_bytes,
};
use wi_note::{Note, set_key};

use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::ops::{changed_claim_fields, has_markers, is_locked};
use crate::paths::{Target, is_engine_internal, is_markdown, normalize};
use crate::publish::{Change, Expect, request_digest};
use crate::vault::{Ingress, Vault, WriteResult};
use crate::work::{WorkKind, WorkRequest};

/// What ingress did with one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum IngestOutcome {
    /// Published as an `ingest` record.
    Ingested { seq: u64 },
    /// Bytes match the engine's latest known version (correlated).
    Unchanged,
    /// A tracked file is gone: recorded as missing.
    Missing,
    /// Not a vault file the engine tracks.
    Skipped { reason: String },
}

/// Result of ingesting one path.
#[derive(Debug, Clone, Serialize)]
pub struct IngestReport {
    pub path: String,
    #[serde(flatten)]
    pub outcome: IngestOutcome,
    pub conflict_markers: bool,
    /// Engine bookkeeping write that set `origin` / `status` (lifecycle).
    pub lifecycle_update: Option<WriteResult>,
    /// Gates run after the file ingress of a memory note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adoption: Option<Box<crate::lifecycle::AdoptReport>>,
}

/// Result of a full reconcile scan.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReconcileReport {
    pub scanned: usize,
    /// Files whose bytes were read and hashed to compare with history.
    #[serde(default)]
    pub hashed: usize,
    /// Temp files of crashed writers (dead pid, never acknowledged) removed.
    pub removed_temps: Vec<String>,
    pub ingested: Vec<IngestReport>,
    pub missing: Vec<String>,
    /// Paths that could not be read or ingested. The scan went on with the
    /// rest, and each one has an `ingest-failed` work request.
    #[serde(default)]
    pub failed: Vec<IngestFailure>,
}

/// One path the scan could not take into history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IngestFailure {
    pub path: String,
    pub error: String,
}

/// The engine's latest knowledge of a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Known {
    Blob(BlobId),
    /// A pending engine move took the file away.
    Removed,
    Unknown,
}

pub(crate) fn known_in_manifest(snap: &ManifestSnapshot, rel: &str) -> Option<Known> {
    for entry in snap.entries.iter().rev() {
        if !matches!(entry.status, OpStatus::Done | OpStatus::Open) {
            continue;
        }
        if entry.op.path() == rel {
            return Some(Known::Blob(entry.op.new_blob().clone()));
        }
        if entry.op.moved_from() == Some(rel) {
            return Some(Known::Removed);
        }
    }
    None
}

impl Vault {
    fn known(&self, snap: &ManifestSnapshot, head: Option<&str>, rel: &str) -> Result<Known> {
        if let Some(known) = known_in_manifest(snap, rel) {
            return Ok(known);
        }
        let Some(head) = head else {
            return Ok(Known::Unknown);
        };
        Ok(History::blob_at(&self.history, head, rel)?.map_or(Known::Unknown, Known::Blob))
    }

    /// Whether file ingress should consider `rel` at all.
    fn ingress_filter(&self, rel: &str) -> Option<String> {
        if is_engine_internal(rel) {
            return Some("engine-internal path".into());
        }
        match self.sync_exclude_oversize(rel) {
            Ok(true) => {
                return Some("larger than 50 MB; kept in the folder outside history".into());
            }
            Err(_) => {
                return Some("large-file exclusion is waiting to be saved; it will retry".into());
            }
            Ok(false) => {
                // A file back under the limit leaves its large-file rule.
                if self.release_large_excludes(Some(&[rel])).is_err() {
                    return Some(
                        "large-file exclusion is waiting to be lifted; it will retry".into(),
                    );
                }
            }
        }
        if wi_index::ignored(self.root(), Path::new(rel)) {
            return Some("ignored by .gitignore or .ignore".into());
        }
        None
    }

    /// Publish an engine-written config file (init) as `cli` ingress.
    pub(crate) fn ingest_engine_file(&self, rel: &str) -> Result<()> {
        self.ingest_bytes(rel, Ingress::Cli)?;
        Ok(())
    }

    /// Track an original imported attachment without rewriting its bytes.
    pub fn ingest_import_source(&self, input: &str, expected_checksum: &str) -> Result<()> {
        let rel = normalize(self.root(), input)?;
        if is_markdown(&rel) || is_engine_internal(&rel) {
            return Err(StoreError::Invalid(
                "import source must be an attachment".into(),
            ));
        }
        if let Some(reason) = self.ingress_filter(&rel) {
            return Err(StoreError::Invalid(reason));
        }
        let (_, bytes, _) = self.ingest_bytes(&rel, Ingress::Import)?;
        let current = bytes.as_deref().map(hash_bytes);
        if current.as_deref() != Some(expected_checksum) {
            return Err(StoreError::Stale {
                path: rel,
                expected: expected_checksum.into(),
                current,
            });
        }
        Ok(())
    }

    /// File ingress for one vault-relative path (watcher event or scan).
    pub fn ingest_path(&self, input: &str) -> Result<IngestReport> {
        let rel = normalize(self.root(), input)?;
        if let Some(reason) = self.ingress_filter(&rel) {
            return Ok(IngestReport {
                path: rel,
                outcome: IngestOutcome::Skipped { reason },
                conflict_markers: false,
                lifecycle_update: None,
                adoption: None,
            });
        }
        if crate::paths::check_no_symlink(self.root(), &rel).is_err() {
            return Ok(IngestReport {
                path: rel,
                outcome: IngestOutcome::Skipped {
                    reason: "symlink".into(),
                },
                conflict_markers: false,
                lifecycle_update: None,
                adoption: None,
            });
        }
        let (outcome, bytes, previous) = self.ingest_bytes(&rel, Ingress::Watcher)?;
        let mut report = IngestReport {
            path: rel.clone(),
            outcome,
            conflict_markers: false,
            lifecycle_update: None,
            adoption: None,
        };
        match &report.outcome {
            IngestOutcome::Ingested { seq } => {
                let seq = *seq;
                let bytes = bytes.unwrap_or_default();
                report.conflict_markers = is_markdown(&rel) && has_markers(&bytes);
                if report.conflict_markers {
                    self.record_work(
                        WorkRequest::new(
                            WorkKind::MergeConflict,
                            &rel,
                            format!("git conflict markers in {}", hash_bytes(&bytes)),
                        )
                        .with_seq(seq),
                    )?;
                } else if is_markdown(&rel) {
                    report.lifecycle_update =
                        self.file_ingress_lifecycle(&rel, &bytes, previous.as_deref(), seq)?;
                }
                let _ = self.index_update(&[rel.as_str()]);
                if !report.conflict_markers && is_markdown(&rel) {
                    report.adoption = self.adopt_after_ingress(&rel)?.map(Box::new);
                }
            }
            IngestOutcome::Missing => {
                // The owner deleted or renamed it: the next flush commits
                // the deletion; history keeps the bytes. No work item.
                self.pending_deletions.borrow_mut().insert(rel.clone());
                let _ = self.with_index(|index| Ok(index.remove_paths(&[rel.as_str()])?));
            }
            _ => {}
        }
        Ok(report)
    }

    /// Compare the file with the engine's latest knowledge and publish an
    /// `ingest` record when they differ. Returns the outcome, the current
    /// bytes and the previous bytes.
    #[allow(clippy::type_complexity)]
    fn ingest_bytes(
        &self,
        rel: &str,
        ingress: Ingress,
    ) -> Result<(IngestOutcome, Option<Vec<u8>>, Option<Vec<u8>>)> {
        self.recover_sync()?;
        for attempt in 0..2 {
            if let Some(hook) = &self.hooks.before_ingest_lock {
                hook(&self.root().join(rel));
            }
            let guard = PublisherLock::acquire(&self.history)?;
            if self.history.wi_dir().join("sync-intent.json").exists() {
                return Err(StoreError::Invalid(
                    "Sync recovery is finishing; retry this save.".into(),
                ));
            }
            let snap = Manifest::snapshot(&self.history, &guard)?;
            match self.admit(&snap, &[rel], attempt == 0) {
                Ok(()) => {}
                Err(StoreError::History(wi_history::HistoryError::OpenIntent { .. }))
                    if attempt == 0 =>
                {
                    drop(guard);
                    self.recover()?;
                    continue;
                }
                // The path waits for recovery; it is ingested once settled.
                Err(StoreError::RecoveryHeld { seq, .. }) => {
                    return Ok((
                        IngestOutcome::Skipped {
                            reason: format!("held by interrupted write {seq}"),
                        },
                        None,
                        None,
                    ));
                }
                Err(e) => return Err(e),
            }
            // Read the file and the branch tip under the lock: a writer that
            // published while this call waited is then seen as known.
            let current = match read_rel(self.root(), rel) {
                Ok(current) => current,
                Err(StoreError::Invalid(_) | StoreError::Protected { .. }) => {
                    return Ok((
                        IngestOutcome::Skipped {
                            reason: "not a regular file".into(),
                        },
                        None,
                        None,
                    ));
                }
                Err(e) => return Err(e),
            };
            let head = History::head(&self.history)?;
            let known = self.known(&snap, head.as_deref(), rel)?;
            let Some(bytes) = current else {
                let outcome = match known {
                    // A committed name this device cannot create (a long
                    // name from a peer) is absent here, not deleted.
                    Known::Blob(_) if !self.creatable(rel) => IngestOutcome::Skipped {
                        reason: "this device cannot create this file name; it stays in history"
                            .into(),
                    },
                    Known::Blob(_) => IngestOutcome::Missing,
                    _ => IngestOutcome::Skipped {
                        reason: "absent".into(),
                    },
                };
                return Ok((outcome, None, None));
            };
            let blob = blob_id_of(&bytes)?;
            if known == Known::Blob(blob.clone()) {
                return Ok((IngestOutcome::Unchanged, Some(bytes), None));
            }
            let previous = match &known {
                Known::Blob(old) => self.history.read_blob(old).ok(),
                _ => None,
            };
            let new_blob = Blob::write_durable(&self.history, &bytes)?;
            let seq = Manifest::append_ingest(
                &self.history,
                &guard,
                IngestRecord {
                    actor: ingress.actor().to_owned(),
                    path: rel.to_owned(),
                    new_hash: hash_bytes(&bytes),
                    new_blob,
                    batch: self.import_mark(ingress),
                },
            )?;
            return Ok((IngestOutcome::Ingested { seq }, Some(bytes), previous));
        }
        Err(StoreError::Invalid("ingest retry exhausted".into()))
    }

    /// Origin rules for file ingress on a lifecycle note (AUTHORITY,
    /// FORMAT): provenance is read from the note itself, never from the
    /// channel that delivered the bytes (an editor, Obsidian, a file-sync
    /// service or a git merge look the same). A note without `origin` gets
    /// `origin.kind=user`: a new note, or one whose agent label the owner
    /// removed (adoption). A written `origin` stays as written, so an
    /// agent-origin note edited or synced in stays agent-origin, and
    /// nothing is ever downgraded. A change to claim content resets a memory
    /// note to `candidate`. The change is a byte-preserving engine write;
    /// lifecycle evaluation runs the gates from the recorded `gate` work
    /// request.
    fn file_ingress_lifecycle(
        &self,
        rel: &str,
        bytes: &[u8],
        previous: Option<&[u8]>,
        seq: u64,
    ) -> Result<Option<WriteResult>> {
        if crate::paths::is_under(rel, self.config.work_dir()) && !self.owns_work_path(rel) {
            return Ok(None);
        }

        let Ok(note) = Note::parse(bytes) else {
            return Ok(None);
        };
        let kind = note
            .frontmatter()
            .and_then(|f| f.get("type"))
            .and_then(|v| v.as_str().map(str::to_owned));
        let Some(kind) = kind.filter(|k| ["memory", "capture", "work"].contains(&k.as_str()))
        else {
            return Ok(None);
        };
        // `lifecycle: off`: lifecycle keys stay as the owner wrote them.
        if !self.lifecycle_on() {
            return Ok(None);
        }
        let origin_kind = note
            .frontmatter()
            .and_then(|f| f.get("origin"))
            .and_then(|o| o.get("kind").and_then(|k| k.as_str().map(str::to_owned)));
        let previous = previous.and_then(|p| Note::parse(p).ok());
        let claim_changed = previous
            .as_ref()
            .is_some_and(|old| !changed_claim_fields(old, &note).is_empty());
        if claim_changed
            && previous.as_ref().is_some_and(|old| {
                old.lifecycle().lifecycle_type != wi_note::LifecycleType::None
                    && old
                        .frontmatter()
                        .and_then(|f| f.get("status"))
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .is_some_and(|s| matches!(s.as_str(), "superseded" | "rejected"))
            })
        {
            self.record_work(
                WorkRequest::new(
                    WorkKind::Gate,
                    rel,
                    "terminal-note: preserve terminal status; create a fork with restores",
                )
                .with_seq(seq),
            )?;
            return Err(StoreError::TerminalNote {
                path: rel.to_owned(),
            });
        }
        let created = previous.is_none();
        let labelled = agent_labelled(&note);
        // The owner removed (or rewrote) the agent label: adoption.
        let adopted = previous.as_ref().is_some_and(agent_labelled) && !labelled;
        let mut set = Vec::new();
        // A retained agent label (`origin.kind: agent` or `noted_by: agent`)
        // keeps the note agent content; only its removal adopts it.
        if origin_kind.is_none() && !labelled && (created || claim_changed || adopted) {
            let origin = json!({
                "kind": "user",
                "locator": "file",
                "captured_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            });
            set.push(("origin", origin));
        }
        let status = note
            .frontmatter()
            .and_then(|f| f.get("status"))
            .and_then(|v| v.as_str().map(str::to_owned));
        if claim_changed && kind == "memory" && status.as_deref().is_some_and(|s| s != "candidate")
        {
            set.push(("status", json!("candidate")));
        }
        if !claim_changed && !created && !adopted {
            return Ok(None);
        }
        self.record_work(
            WorkRequest::new(
                WorkKind::Gate,
                rel,
                format!("file ingress of {}", hash_bytes(bytes)),
            )
            .with_seq(seq),
        )?;
        if set.is_empty()
            || is_locked(&note)
            || note.frontmatter().is_some_and(|f| f.is_editable().is_err())
        {
            return Ok(None);
        }
        let mut edited = note.clone();
        for (key, value) in &set {
            edited = set_key(&edited, key, value).map_err(|e| StoreError::Edit(e.to_string()))?;
        }
        let target = Target {
            rel: rel.to_owned(),
            abs: self.root().join(rel),
        };
        let digest =
            request_digest(&json!({"tool": "file-ingress-lifecycle", "path": rel, "seq": seq}));
        match self.publish(Change {
            target,
            bytes: edited.to_bytes().to_vec(),
            expect: Expect::Hash(hash_bytes(bytes)),
            moved_from: None,
            op_id: None,
            digest,
            ingress: Ingress::Engine,
            revert_of: None,
            charge: None,
        }) {
            Ok(result) => Ok(Some(result)),
            // The owner saved again meanwhile; the next event handles it.
            Err(StoreError::Stale { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Remove engine temp files (`.<name>.wi-tmp-<pid>-<n>`) whose writer
    /// is no longer running and that no open intent or `recovery-problem`
    /// request names. Such a temp holds only bytes the engine never
    /// acknowledged. Returns the removed paths.
    pub(crate) fn remove_dead_temps(&self) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        // A temp that recovery or a write kept because it could not be
        // settled (a `recovery-problem` names it, or it belongs to an
        // intent held open) may hold the only copy of editor bytes.
        let kept: Vec<String> = self
            .work_requests()?
            .into_iter()
            .filter(|w| w.kind == crate::work::WorkKind::RecoveryProblem)
            .map(|w| w.path)
            .collect();
        let open_targets: Vec<String> = {
            let guard = wi_history::PublisherLock::acquire(&self.history)?;
            wi_history::Manifest::snapshot(&self.history, &guard)
                .map(|snap| {
                    snap.open_intents()
                        .map(|e| e.op.path().to_owned())
                        .collect()
                })
                .unwrap_or_default()
        };
        for (rel, pid) in crate::doctor::scratch_temps(self.root()) {
            let held = kept.contains(&rel)
                || open_targets.iter().any(|target| {
                    let (dir, name) = match target.rsplit_once('/') {
                        Some((dir, name)) => (format!("{dir}/"), name),
                        None => (String::new(), target.as_str()),
                    };
                    rel.starts_with(&format!("{dir}.{name}{}", crate::paths::TMP_MARK))
                });
            if held {
                continue;
            }
            let path = self.root().join(&rel);
            // Only a regular file is a writer's temp; anything else under
            // that name is left for doctor to report.
            let regular = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
            if regular && crate::doctor::process_gone(pid) {
                std::fs::remove_file(&path).map_err(|e| StoreError::io(&path, e))?;
                removed.push(rel);
            }
        }
        Ok(removed)
    }

    /// Scan the whole vault for file ingress the engine has not seen
    /// (no watcher running): hash every eligible file, ingest changed ones,
    /// record missing ones and refresh the ingress-stat cache.
    pub fn reconcile(&self) -> Result<ReconcileReport> {
        let mut report = ReconcileReport {
            removed_temps: self.remove_dead_temps()?,
            ..ReconcileReport::default()
        };
        // Files that shrank back under the size limit are walked again.
        self.release_large_excludes(None)?;
        let survey = self.survey(crate::ingress::Scope::All)?;
        self.ingest_survey(survey, &mut report)?;
        for path in self.record_missing()? {
            if !report.missing.contains(&path) {
                report.missing.push(path);
            }
        }
        Ok(report)
    }

    /// Commit every committed path that is gone from the working tree as
    /// an owner deletion (`wi commit`, `wi reindex`, maintenance). The
    /// content stays recoverable from git history. Returns the paths.
    pub(crate) fn record_missing(&self) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        // The second round sees notes created and deleted before a flush:
        // the first round's flush committed them.
        for round in 0..2 {
            if round == 1 {
                wi_history::Flusher::flush(&self.history)?;
            }
            {
                let mut pending = self.pending_deletions.borrow_mut();
                for entry in Missing::scan(&self.history)? {
                    // A committed name this device cannot create (a long
                    // name from a peer) is absent here, not deleted.
                    if self.creatable(&entry.path) && self.ingress_filter(&entry.path).is_none() {
                        pending.insert(entry.path);
                    }
                }
            }
            let planned = self.plan_deletions()?;
            removed.extend(self.record_deletions(&planned)?);
        }
        Ok(removed)
    }

    /// Take the deletions file ingress found, each with the path its bytes
    /// were published again under (a rename), judged before a flush
    /// retires those records.
    pub(crate) fn plan_deletions(&self) -> Result<Vec<Deletion>> {
        let paths: Vec<String> = std::mem::take(&mut *self.pending_deletions.borrow_mut())
            .into_iter()
            .collect();
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let snap = Manifest::snapshot_unlocked(&self.history)?;
        let head = History::head(&self.history)?;
        let mut deletions = Vec::new();
        for path in paths {
            let blob = match known_in_manifest(&snap, &path) {
                Some(Known::Blob(blob)) => Some(blob),
                _ => match head.as_deref() {
                    Some(head) => History::blob_at(&self.history, head, &path)?,
                    None => None,
                },
            };
            let renamed_to = blob.and_then(|blob| {
                snap.entries
                    .iter()
                    .rev()
                    .find(|e| {
                        e.status == OpStatus::Done
                            && e.op.path() != path
                            && *e.op.new_blob() == blob
                            && self.root().join(e.op.path()).is_file()
                    })
                    .map(|e| e.op.path().to_owned())
            });
            deletions.push(Deletion { path, renamed_to });
        }
        Ok(deletions)
    }

    /// Commit planned deletions as one owner commit (actor `watcher`). A
    /// path that came back or that a pending record still touches stays
    /// out (the file is then current again, or the next flush settles it).
    pub(crate) fn record_deletions(&self, deletions: &[Deletion]) -> Result<Vec<String>> {
        if deletions.is_empty() {
            return Ok(Vec::new());
        }
        let actor = Ingress::Watcher.actor();
        Ok(History::record_deletions(&self.history, deletions, actor)?
            .map(|(_, removed)| removed)
            .unwrap_or_default())
    }

    /// Plan and commit the deletions file ingress found.
    pub(crate) fn commit_pending_deletions(&self) -> Result<Vec<String>> {
        let planned = self.plan_deletions()?;
        self.record_deletions(&planned)
    }
}

/// Every regular file under `root` that git would see (nested
/// `.gitignore` honoured), excluding `.git` and engine scratch files.
pub(crate) fn walk_files(root: &Path) -> Vec<String> {
    walk_files_in(root, root)
}

/// Walk a changed directory using the vault's ignore rules, without
/// descending into unrelated subtrees. Starting at the root preserves
/// `.gitignore` rules inherited by a newly created or moved directory.
pub(crate) fn walk_files_in(root: &Path, directory: &Path) -> Vec<String> {
    let mut out: Vec<String> = walker(root, directory)
        .build()
        .flatten()
        .filter_map(|entry| walked_file(root, &entry))
        .collect();
    out.sort();
    out
}

/// [`walk_files`] reading directories on every core: the same files, in
/// the same order. Directory traversal runs in parallel for large vaults.
///
/// `visit` runs on the walker threads for each file's absolute path;
/// files it maps to `None` are left out. Results are
/// sorted by path.
pub(crate) fn walk_files_parallel<T: Send>(
    root: &Path,
    visit: impl Fn(&Path) -> Option<T> + Sync,
) -> Vec<(String, T)> {
    /// One walker thread's files, handed over when the thread finishes.
    struct Sink<'a, T> {
        local: Vec<(String, T)>,
        found: &'a Mutex<Vec<(String, T)>>,
    }
    impl<T> Drop for Sink<'_, T> {
        fn drop(&mut self) {
            self.found
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .append(&mut self.local);
        }
    }
    use std::sync::{Mutex, PoisonError};
    let found = Mutex::new(Vec::new());
    walker(root, root).build_parallel().run(|| {
        let mut sink = Sink {
            local: Vec::new(),
            found: &found,
        };
        let visit = &visit;
        Box::new(move |entry| {
            if let Ok(entry) = entry
                && let Some(rel) = walked_file(root, &entry)
                && let Some(value) = visit(entry.path())
            {
                sink.local.push((rel, value));
            }
            ignore::WalkState::Continue
        })
    });
    let mut out = found.into_inner().unwrap_or_else(PoisonError::into_inner);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The walk of [`walk_files_in`]: the vault's ignore rules, no symlinks,
/// no engine-internal paths, only `directory` and the chain to it.
fn walker(root: &Path, directory: &Path) -> ignore::WalkBuilder {
    let owned = root.to_path_buf();
    let directory = directory.to_path_buf();
    // Folder mode: the `.gitignore` rules live in `<gitdir>/info/exclude`,
    // below the vault's own ignore files.
    let exclude = wi_index::exclude_matcher(root);
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .require_git(false)
        .filter_entry(move |entry| {
            if !entry.path().starts_with(&directory) && !directory.starts_with(entry.path()) {
                return false;
            }
            entry
                .path()
                .strip_prefix(&owned)
                .map(|rel| {
                    let rel = rel.to_string_lossy();
                    rel.is_empty() || !is_engine_internal(&rel)
                })
                .unwrap_or(false)
                && !exclude.as_ref().is_some_and(|m| {
                    wi_index::walk_excluded(
                        &owned,
                        m,
                        entry.path(),
                        entry.file_type().is_some_and(|t| t.is_dir()),
                    )
                })
        });
    builder
}

/// A walked regular file as a vault-relative UTF-8 path.
fn walked_file(root: &Path, entry: &ignore::DirEntry) -> Option<String> {
    if !entry.file_type().is_some_and(|t| t.is_file()) {
        return None;
    }
    Some(entry.path().strip_prefix(root).ok()?.to_str()?.to_owned())
}

/// Whether a note carries an agent label in either supported form:
/// `origin.kind: agent` or `noted_by: agent` (AUTHORITY).
fn agent_labelled(note: &Note) -> bool {
    let Some(f) = note.frontmatter() else {
        return false;
    };
    let origin_agent = f
        .get("origin")
        .and_then(|o| o.get("kind").and_then(|k| k.as_str().map(str::to_owned)))
        .as_deref()
        == Some("agent");
    origin_agent
        || f.get("noted_by")
            .and_then(|v| v.as_str().map(str::to_owned))
            .as_deref()
            == Some("agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parallel_walk_lists_exactly_the_sequential_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (rel, text) in [
            ("a.md", "a"),
            ("x/b.md", "b"),
            ("x/y/z/c.md", "c"),
            ("x/.gitignore", "skipped/\n*.tmp\n"),
            ("x/skipped/d.md", "d"),
            ("x/y/e.tmp", "e"),
            (".git/HEAD", "ref"),
            ("v/w/f.png", "f"),
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let sequential = walk_files(root);
        let parallel: Vec<String> = walk_files_parallel(root, |p| Some(p.to_path_buf()))
            .into_iter()
            .map(|(rel, abs)| {
                assert_eq!(abs, root.join(&rel));
                rel
            })
            .collect();
        assert_eq!(parallel, sequential);
        assert!(sequential.contains(&"x/y/z/c.md".to_owned()));
        assert!(
            !sequential
                .iter()
                .any(|p| p.contains("skipped") || p.ends_with(".tmp"))
        );
    }
}
