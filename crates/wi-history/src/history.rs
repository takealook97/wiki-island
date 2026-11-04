//! Per-path history, reading old bytes and revert as a new commit.

use std::collections::{BTreeMap, HashSet};

use gix::ObjectId;
use gix::bstr::ByteSlice;
use serde::Serialize;

use crate::error::{HistoryError, Result};
use crate::flush::Flusher;
use crate::gitops::{self, TreeEdit};
use crate::lock::{FlushLock, PublisherLock};
use crate::manifest::Manifest;
use crate::vault::{BlobId, Vault, validate_rel_path};

/// A commit that changed a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryEntry {
    pub commit: String,
    /// Blob at the path after this commit; `None` when the commit removed it.
    pub blob: Option<BlobId>,
    pub author_time: i64,
    pub summary: String,
    /// `Wi-*` trailers in message order.
    pub trailers: Vec<(String, String)>,
}

/// A path deleted outside the engine, for [`History::record_deletions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Deletion {
    pub path: String,
    /// The path the same bytes now live at, when the deletion was a rename.
    pub renamed_to: Option<String>,
}

/// A committed operation found by its `Wi-Operation` trailer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationHit {
    pub commit: String,
    pub request_digest: String,
    /// First sequence id of the commit's `Wi-Seq` trailer.
    pub seq: Option<u64>,
    /// Path named by the commit subject.
    pub path: String,
    /// Blob of `path` in that commit (`None` if the path is gone there).
    pub blob: Option<BlobId>,
    /// The commit's `Wi-Moved-From` trailer.
    pub moved_from: Option<String>,
}

/// One `Wi-Operation` trailer of a commit (no blob lookup).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationTrailer {
    pub op_id: String,
    pub request_digest: String,
    pub commit: String,
    pub seq: Option<u64>,
    pub path: String,
    pub moved_from: Option<String>,
}

/// Operations of the commits from the branch tip back to a stop commit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OperationScan {
    /// Branch tip when the scan started (`None` when unborn).
    pub tip: Option<String>,
    /// Trailers, newest commit first.
    pub operations: Vec<OperationTrailer>,
    /// Whether the walk met the stop commit (`false`: it walked to the root,
    /// so the stop commit is not a first-parent ancestor of the tip).
    pub reached_stop: bool,
}

/// The `Wi-Operation` trailers of one commit.
fn commit_operations(id: ObjectId, message: &str) -> Vec<OperationTrailer> {
    let trailers = gitops::trailers(message);
    let subject = message.lines().next().unwrap_or_default();
    let path = subject
        .strip_prefix("wi: ")
        .and_then(|rest| rest.split_once(' '))
        .map(|(_, path)| path.to_owned())
        .unwrap_or_default();
    let seq = trailers
        .iter()
        .find(|(k, _)| k == crate::TRAILER_SEQ)
        .and_then(|(_, v)| gitops::parse_seq_trailer(v))
        .and_then(|(seqs, _)| seqs.first().copied());
    let moved_from = trailers
        .iter()
        .find(|(k, _)| k == crate::TRAILER_MOVED_FROM)
        .map(|(_, v)| v.clone());
    trailers
        .iter()
        .filter(|(k, _)| k == crate::TRAILER_OPERATION)
        .filter_map(|(_, v)| v.split_once(' '))
        .map(|(op_id, digest)| OperationTrailer {
            op_id: op_id.to_owned(),
            request_digest: digest.to_owned(),
            commit: id.to_string(),
            seq,
            path: path.clone(),
            moved_from: moved_from.clone(),
        })
        .collect()
}

/// Compare only changed subtrees along requested prefixes. Blob identity,
/// rather than executable bits, matches the existing single-path query.
fn changed_blobs(
    vault: &Vault,
    after: Option<ObjectId>,
    before: Option<ObjectId>,
    prefixes: &HashSet<String>,
) -> Result<BTreeMap<String, Option<ObjectId>>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(String::new(), after, before)];
    while let Some((prefix, after, before)) = stack.pop() {
        if after == before {
            continue;
        }
        let entries = |id: Option<ObjectId>| -> Result<BTreeMap<String, (ObjectId, bool, bool)>> {
            let mut entries = BTreeMap::new();
            if let Some(id) = id {
                let tree = vault.repo().find_tree(id).map_err(HistoryError::git)?;
                for entry in tree.decode().map_err(HistoryError::git)?.entries {
                    // Requested paths are UTF-8, so other names cannot match.
                    if let Ok(name) = entry.filename.to_str() {
                        entries.insert(
                            name.to_owned(),
                            (
                                entry.oid.to_owned(),
                                entry.mode.is_tree(),
                                entry.mode.is_blob(),
                            ),
                        );
                    }
                }
            }
            Ok(entries)
        };
        let after = entries(after)?;
        let before = entries(before)?;
        let names: std::collections::BTreeSet<_> = after.keys().chain(before.keys()).collect();
        for name in names {
            let path = format!("{prefix}{name}");
            if !prefixes.contains(&path) {
                continue;
            }
            let new = after.get(name);
            let old = before.get(name);
            let blob = |e: Option<&(ObjectId, bool, bool)>| e.filter(|e| e.2).map(|e| e.0);
            if blob(new) != blob(old) {
                out.insert(path.clone(), blob(new));
            }
            let tree = |e: Option<&(ObjectId, bool, bool)>| e.filter(|e| e.1).map(|e| e.0);
            if tree(new) != tree(old) {
                stack.push((format!("{path}/"), tree(new), tree(old)));
            }
        }
    }
    Ok(out)
}

fn parse_commit(vault: &Vault, commit: &str) -> Result<ObjectId> {
    let id = ObjectId::from_hex(commit.as_bytes()).map_err(|_| HistoryError::InvalidRecord {
        field: "commit",
        reason: format!("{commit:?} is not a full commit id"),
    })?;
    vault.repo().find_commit(id).map_err(HistoryError::git)?;
    Ok(id)
}

/// History queries and revert.
pub struct History;

impl History {
    /// Current branch tip, `None` when the branch has no commits.
    pub fn head(vault: &Vault) -> Result<Option<String>> {
        Ok(gitops::branch_tip(vault)?.1.map(|id| id.to_string()))
    }

    /// Commits reachable from the tip that changed `path`, newest first.
    /// Renames are not followed: a moved note's earlier history is under its
    /// old path (`Wi-Moved-From` names it).
    pub fn log(vault: &Vault, path: &str) -> Result<Vec<HistoryEntry>> {
        validate_rel_path(path)?;
        let Some(tip) = gitops::branch_tip(vault)?.1 else {
            return Ok(Vec::new());
        };
        let repo = vault.repo();
        let walk = repo.rev_walk([tip]).all().map_err(HistoryError::git)?;
        let mut out = Vec::new();
        for info in walk {
            let info = info.map_err(HistoryError::git)?;
            let id = info.id;
            let here = gitops::blob_at(vault, id, path)?;
            let (message, parents) = gitops::commit_message(vault, id)?;
            // Changed if it differs from every parent (a root: if present).
            let mut changed = parents.is_empty() && here.is_some();
            if !parents.is_empty() {
                changed = true;
                for parent in &parents {
                    changed &= gitops::blob_at(vault, *parent, path)? != here;
                }
            }
            if !changed {
                continue;
            }
            let commit = repo.find_commit(id).map_err(HistoryError::git)?;
            let author_time = commit
                .author()
                .map_err(HistoryError::git)?
                .time()
                .map(|t| t.seconds)
                .unwrap_or(0);
            out.push(HistoryEntry {
                commit: id.to_string(),
                blob: here.map(|b| BlobId(b.to_string())),
                author_time,
                summary: message.lines().next().unwrap_or_default().to_owned(),
                trailers: gitops::trailers(&message),
            });
        }
        Ok(out)
    }

    /// Query several paths in input order with one graph walk. Entries and
    /// merge semantics match `log`; renames are not followed. A limit applies
    /// independently to each path, including duplicate input paths.
    pub fn log_many(
        vault: &Vault,
        paths: &[&str],
        limit_per_path: Option<usize>,
    ) -> Result<Vec<(String, Vec<HistoryEntry>)>> {
        let mut logs = BTreeMap::<String, Vec<HistoryEntry>>::new();
        let mut prefixes = HashSet::new();
        for path in paths {
            validate_rel_path(path)?;
            logs.entry((*path).to_owned()).or_default();
            let mut prefix = String::new();
            for part in path.split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                prefixes.insert(prefix.clone());
            }
        }
        let limit = limit_per_path.unwrap_or(usize::MAX);
        if !logs.is_empty()
            && limit > 0
            && let Some(tip) = gitops::branch_tip(vault)?.1
        {
            let repo = vault.repo();
            let walk = repo.rev_walk([tip]).all().map_err(HistoryError::git)?;
            for info in walk {
                let id = info.map_err(HistoryError::git)?.id;
                let (message, parents) = gitops::commit_message(vault, id)?;
                let tree = gitops::tree_of(vault, Some(id))?;
                // Intersect parent diffs: like log(), a merge counts only
                // when the resulting blob differs from every parent.
                let mut changed = None::<BTreeMap<String, Option<ObjectId>>>;
                for parent in parents
                    .iter()
                    .copied()
                    .map(Some)
                    .chain(parents.is_empty().then_some(None))
                {
                    let before = parent
                        .map(|p| gitops::tree_of(vault, Some(p)))
                        .transpose()?;
                    let diff = changed_blobs(vault, Some(tree), before, &prefixes)?;
                    if let Some(changed) = &mut changed {
                        changed.retain(|path, _| diff.contains_key(path));
                    } else {
                        changed = Some(diff);
                    }
                    if changed.as_ref().is_some_and(BTreeMap::is_empty) {
                        break;
                    }
                }
                let changed = changed.unwrap_or_default();
                if changed
                    .keys()
                    .any(|p| logs.get(p).is_some_and(|v| v.len() < limit))
                {
                    let commit = repo.find_commit(id).map_err(HistoryError::git)?;
                    let author_time = commit
                        .author()
                        .map_err(HistoryError::git)?
                        .time()
                        .map(|t| t.seconds)
                        .unwrap_or(0);
                    let entry = HistoryEntry {
                        commit: id.to_string(),
                        blob: None,
                        author_time,
                        summary: message.lines().next().unwrap_or_default().to_owned(),
                        trailers: gitops::trailers(&message),
                    };
                    for (path, blob) in changed {
                        if let Some(log) = logs.get_mut(&path).filter(|v| v.len() < limit) {
                            let mut entry = entry.clone();
                            entry.blob = blob.map(|id| BlobId(id.to_string()));
                            log.push(entry);
                        }
                    }
                }
                if logs.values().all(|v| v.len() >= limit) {
                    break;
                }
            }
        }
        Ok(paths
            .iter()
            .map(|path| ((*path).to_owned(), logs[*path].clone()))
            .collect())
    }

    /// The first-parent commit whose `Wi-Operation` trailer names `op_id`,
    /// newest first. Engine commits hold one operation each, so the subject
    /// `wi: <actor> <path>` names the path.
    pub fn find_operation(vault: &Vault, op_id: &str) -> Result<Option<OperationHit>> {
        let Some(mut next) = gitops::branch_tip(vault)?.1 else {
            return Ok(None);
        };
        loop {
            let (message, parents) = gitops::commit_message(vault, next)?;
            if let Some(found) = commit_operations(next, &message)
                .into_iter()
                .find(|t| t.op_id == op_id)
            {
                return Ok(Some(Self::operation_hit(vault, found)?));
            }
            match parents.first() {
                Some(parent) => next = *parent,
                None => return Ok(None),
            }
        }
    }

    /// Resolve a trailer to a hit with the blob of its path in its commit.
    pub fn operation_hit(vault: &Vault, trailer: OperationTrailer) -> Result<OperationHit> {
        let blob = if trailer.path.is_empty() {
            None
        } else {
            let id = parse_commit(vault, &trailer.commit)?;
            gitops::blob_at(vault, id, &trailer.path)?.map(|b| BlobId(b.to_string()))
        };
        Ok(OperationHit {
            commit: trailer.commit,
            request_digest: trailer.request_digest,
            seq: trailer.seq,
            path: trailer.path,
            blob,
            moved_from: trailer.moved_from,
        })
    }

    /// Every `Wi-Operation` trailer on the first-parent chain from the
    /// branch tip back to (excluding) `stop`. A disposable op-id map uses it
    /// to scan only the commits added since its last scan.
    pub fn operations_since(vault: &Vault, stop: Option<&str>) -> Result<OperationScan> {
        let Some(tip) = gitops::branch_tip(vault)?.1 else {
            return Ok(OperationScan {
                reached_stop: stop.is_none(),
                ..OperationScan::default()
            });
        };
        let stop = stop.and_then(|s| ObjectId::from_hex(s.as_bytes()).ok());
        let mut scan = OperationScan {
            tip: Some(tip.to_string()),
            ..OperationScan::default()
        };
        let mut next = tip;
        loop {
            if Some(next) == stop {
                scan.reached_stop = true;
                return Ok(scan);
            }
            let (message, parents) = gitops::commit_message(vault, next)?;
            scan.operations.extend(commit_operations(next, &message));
            match parents.first() {
                Some(parent) => next = *parent,
                None => return Ok(scan),
            }
        }
    }

    /// `Wi-Loss` trailer values of first-parent commits authored at or after
    /// `since` (unix seconds), newest first. A commit time later than the
    /// event it records, so the walk stops at the first older commit. A
    /// commit stamped in the future (a clock moved backward) is included.
    pub fn losses_since(vault: &Vault, since: i64) -> Result<Vec<String>> {
        let Some(mut next) = gitops::branch_tip(vault)?.1 else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        loop {
            if gitops::commit_time(vault, next)? < since {
                return Ok(out);
            }
            let (message, parents) = gitops::commit_message(vault, next)?;
            out.extend(
                gitops::trailers(&message)
                    .into_iter()
                    .filter(|(k, _)| k == crate::TRAILER_LOSS)
                    .map(|(_, v)| v),
            );
            match parents.first() {
                Some(parent) => next = *parent,
                None => return Ok(out),
            }
        }
    }

    /// Author time (unix seconds) of `commit`.
    pub fn commit_time(vault: &Vault, commit: &str) -> Result<i64> {
        gitops::commit_time(vault, parse_commit(vault, commit)?)
    }

    /// Every file at the branch tip with its blob id (empty when unborn).
    pub fn tree(vault: &Vault) -> Result<Vec<(String, BlobId)>> {
        let Some(tip) = gitops::branch_tip(vault)?.1 else {
            return Ok(Vec::new());
        };
        Ok(gitops::tree_blobs(vault, tip)?
            .into_iter()
            .map(|(path, blob)| (path, BlobId(blob.to_string())))
            .collect())
    }

    /// Blob id of `path` at `commit`.
    pub fn blob_at(vault: &Vault, commit: &str, path: &str) -> Result<Option<BlobId>> {
        validate_rel_path(path)?;
        let id = parse_commit(vault, commit)?;
        Ok(gitops::blob_at(vault, id, path)?.map(|b| BlobId(b.to_string())))
    }

    /// Bytes of `path` at `commit`.
    pub fn read_at(vault: &Vault, commit: &str, path: &str) -> Result<Vec<u8>> {
        let blob =
            Self::blob_at(vault, commit, path)?.ok_or_else(|| HistoryError::NotInCommit {
                path: path.to_owned(),
                commit: commit.to_owned(),
            })?;
        vault.read_blob(&blob)
    }

    /// Whether `ancestor` is reachable from `descendant` (or equal to it).
    pub fn is_ancestor(vault: &Vault, ancestor: &str, descendant: &str) -> Result<bool> {
        let a = parse_commit(vault, ancestor)?;
        let d = parse_commit(vault, descendant)?;
        if a == d {
            return Ok(true);
        }
        let walk = vault
            .repo()
            .rev_walk([d])
            .all()
            .map_err(HistoryError::git)?;
        for info in walk {
            if info.map_err(HistoryError::git)?.id == a {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Commit deletions made outside the engine (an editor, a file manager,
    /// a shell) as one commit on top of the branch, attributed to `actor`
    /// with a `Wi-Deleted` trailer per path (`Wi-Renamed` with `<from> ->
    /// <to>` when the bytes reappeared under another path). Pending records
    /// are flushed first; a path a remaining record still touches, a path
    /// that exists again, or a path the branch tip does not hold is left
    /// out. The publisher lock is held from the check through the ref
    /// update, so no engine write can recreate a path in between. Returns
    /// the commit and the paths it removed, or `None` when nothing was left
    /// to remove. The old bytes stay recoverable from history.
    pub fn record_deletions(
        vault: &Vault,
        deletions: &[Deletion],
        actor: &str,
    ) -> Result<Option<(String, Vec<String>)>> {
        if actor.is_empty() || actor.chars().any(char::is_whitespace) {
            return Err(HistoryError::InvalidRecord {
                field: "actor",
                reason: format!("{actor:?} must be non-empty without whitespace"),
            });
        }
        for deletion in deletions {
            validate_rel_path(&deletion.path)?;
        }
        let lock = FlushLock::acquire(vault)?;
        Flusher::flush_locked(vault, &lock)?;
        let guard = PublisherLock::acquire(vault)?;
        let pending = Manifest::snapshot(vault, &guard)?;
        let (branch, tip) = gitops::branch_tip(vault)?;
        let Some(tip_id) = tip else {
            return Ok(None);
        };
        let mut removed: Vec<&Deletion> = Vec::new();
        for deletion in deletions {
            let path = deletion.path.as_str();
            let on_disk = vault.root().join(path);
            match std::fs::symlink_metadata(&on_disk) {
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(HistoryError::io(on_disk, e)),
            }
            if pending.entries.iter().any(|e| e.op.touches(&[path]))
                || gitops::blob_at(vault, tip_id, path)?.is_none()
                || removed.iter().any(|d| d.path == path)
            {
                continue;
            }
            removed.push(deletion);
        }
        if removed.is_empty() {
            return Ok(None);
        }
        let subject = match removed.as_slice() {
            [one] => match &one.renamed_to {
                Some(to) => format!("wi: {actor} rename {} -> {to}", one.path),
                None => format!("wi: {actor} delete {}", one.path),
            },
            many => format!("wi: {actor} delete {} files", many.len()),
        };
        let mut lines = vec![
            subject,
            String::new(),
            format!("{}: {actor}", crate::TRAILER_ACTOR),
        ];
        for deletion in &removed {
            lines.push(match &deletion.renamed_to {
                Some(to) => format!("{}: {} -> {to}", crate::TRAILER_RENAMED, deletion.path),
                None => format!("{}: {}", crate::TRAILER_DELETED, deletion.path),
            });
        }
        let mut message = lines.join("\n");
        message.push('\n');
        let edits: Vec<TreeEdit<'_>> = removed
            .iter()
            .map(|d| TreeEdit::Remove { path: &d.path })
            .collect();
        let paths: Vec<&str> = removed.iter().map(|d| d.path.as_str()).collect();
        let (new, _) = gitops::write_commit(vault, tip, &edits, &message, gitops::now_secs())?;
        gitops::sync_commit(vault, new, &paths)?;
        gitops::advance_branch(vault, &branch, tip, new, "wi: record deletions")?;
        // Best effort, as in a flush: the commit is durable either way.
        let _ = gitops::sync_git_index(vault, new);
        drop(guard);
        Ok(Some((
            new.to_string(),
            paths.into_iter().map(str::to_owned).collect(),
        )))
    }

    /// Restore the bytes `path` had at `commit` as a new commit on top of the
    /// branch, with `Wi-Revert` and `Wi-Actor` trailers. Pending manifest
    /// records are flushed first; if any remain (an open intent, an
    /// unacknowledged conflict, an incomplete batch, or a record appended
    /// meanwhile) the revert fails with `PendingFlush` and changes nothing,
    /// so history never moves ahead of acknowledged operations. The
    /// publisher lock is held from that check through the ref update.
    /// Earlier commits are never touched.
    ///
    /// This changes history only; the caller writes the restored bytes
    /// to the working tree through the write path. The preferred engine path
    /// is a normal write whose `IntentRecord::revert_of` names the commit.
    pub fn revert(vault: &Vault, path: &str, commit: &str, actor: &str) -> Result<String> {
        validate_rel_path(path)?;
        if actor.is_empty() || actor.chars().any(char::is_whitespace) {
            return Err(HistoryError::InvalidRecord {
                field: "actor",
                reason: format!("{actor:?} must be non-empty without whitespace"),
            });
        }
        let source = parse_commit(vault, commit)?;
        let lock = FlushLock::acquire(vault)?;
        let flushed = Flusher::flush_locked(vault, &lock)?;
        let guard = PublisherLock::acquire(vault)?;
        let remaining = Manifest::snapshot(vault, &guard)?.entries.len();
        if remaining > 0 {
            return Err(HistoryError::PendingFlush {
                remaining,
                blocked: flushed.blocked,
            });
        }
        let blob =
            gitops::blob_at(vault, source, path)?.ok_or_else(|| HistoryError::NotInCommit {
                path: path.to_owned(),
                commit: commit.to_owned(),
            })?;
        let (branch, tip) = gitops::branch_tip(vault)?;
        let message = format!(
            "wi: {actor} revert {path}\n\n{}: {source}\n{}: {actor}\n",
            crate::TRAILER_REVERT,
            crate::TRAILER_ACTOR
        );
        let (new, _) = gitops::write_commit(
            vault,
            tip,
            &[TreeEdit::Upsert { path, blob }],
            &message,
            gitops::now_secs(),
        )?;
        gitops::sync_commit(vault, new, &[path])?;
        gitops::advance_branch(vault, &branch, tip, new, &format!("wi: revert {path}"))?;
        // Best effort, as in a flush: the commit is durable either way and
        // the next flush that commits rewrites the index again.
        let _ = gitops::sync_git_index(vault, new);
        drop(guard);
        Ok(new.to_string())
    }
}
