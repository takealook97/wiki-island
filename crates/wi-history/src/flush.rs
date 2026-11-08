//! Flush: one commit per acknowledged operation (one per bulk-import batch
//! run), in sequence order, then retirement of exactly the settled prefix.
//! An open intent is skipped, not waited for: records after it commit
//! first, and it commits once recovery settles it.

use std::collections::{BTreeMap, BTreeSet};

use gix::ObjectId;
use serde::Serialize;

use crate::error::{HistoryError, Result};
use crate::fault;
use crate::gitops::{self, TreeEdit};
use crate::lock::{FlushLock, PublisherLock};
use crate::manifest::{Manifest, ManifestEntry, ManifestSnapshot, OpRecord, OpStatus, Seq};
use crate::vault::Vault;

/// Why a flush stopped before the end of the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum FlushBlock {
    /// An intent without `done`: a crash awaiting replay, or one recovery
    /// holds open. A plain open intent does not stop the flush: later
    /// records (on other paths, see `Manifest::append_intent`) still commit,
    /// but nothing at or after it is retired until it settles. An open
    /// member of a bulk-import batch stops the flush.
    OpenIntent { seq: Seq },
    /// A recovery conflict with no acknowledged work item yet.
    UnackedConflict { seq: Seq },
    /// A bulk-import batch whose last record is not yet appended.
    IncompleteBatch { seq: Seq, batch: String },
    /// A commit names this sequence id of this manifest but does not hold
    /// its record: the id was reused. Nothing at or after it is retired.
    CommittedMismatch { seq: Seq, commit: String },
}

/// One commit written by a flush.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitSummary {
    pub commit: String,
    pub seqs: Vec<Seq>,
}

/// Result of a flush.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FlushReport {
    pub commits: Vec<CommitSummary>,
    /// Records found already committed (by `Wi-Seq`) and only retired.
    pub already_committed: Vec<Seq>,
    /// Retired prefix after the flush.
    pub retired_upto: Seq,
    pub blocked: Option<FlushBlock>,
    /// Unretired operations left in the manifest.
    pub remaining: usize,
    /// Set when the commits are durable but `.git/index` could not be
    /// refreshed (for example `index.lock` held by a git command).
    pub index_warning: Option<String>,
}

struct Group<'a> {
    entries: Vec<&'a ManifestEntry>,
    batch: Option<String>,
    /// Batch members whose own path a newer interleaved record committed
    /// before the batch already wrote or moved away: re-applying the write
    /// would put older bytes back on top.
    stale_upserts: BTreeSet<Seq>,
    /// Batch moves whose source path a newer interleaved record recreated:
    /// removing it would delete that newer file. A move's two path effects
    /// are superseded independently.
    stale_removals: BTreeSet<Seq>,
}

struct Plan<'a> {
    groups: Vec<Group<'a>>,
    already: Vec<Seq>,
    retire_upto: Option<Seq>,
    blocked: Option<FlushBlock>,
}

fn status_block(entry: &ManifestEntry) -> Option<FlushBlock> {
    match &entry.status {
        OpStatus::Open => Some(FlushBlock::OpenIntent { seq: entry.seq }),
        OpStatus::Conflict { acked: false, .. } => {
            Some(FlushBlock::UnackedConflict { seq: entry.seq })
        }
        _ => None,
    }
}

/// What the plan knows about sequence ids already named by commits.
struct Committed {
    /// Named by a commit whose tree and trailers match the record.
    verified: BTreeSet<Seq>,
    /// Named by a commit that does not match the record.
    mismatched: BTreeMap<Seq, ObjectId>,
}

/// Plan commits in sequence order. A record commits alone; a bulk-import
/// batch commits as one commit at the position of its `last` record, so
/// records interleaved with it commit before it and none of them splits it.
/// A batch without its `last` record blocks the flush at its first record.
fn plan<'a>(snap: &'a ManifestSnapshot, committed: &Committed) -> Plan<'a> {
    let mut out = Plan {
        groups: Vec::new(),
        already: Vec::new(),
        retire_upto: None,
        blocked: None,
    };
    let entries = &snap.entries;
    let mut handled: BTreeSet<Seq> = BTreeSet::new();
    let mut waiting: BTreeSet<Seq> = BTreeSet::new();
    let mut pending: BTreeMap<Seq, Group<'a>> = BTreeMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        if let Some(group) = pending.remove(&entry.seq) {
            emit(&mut out, &mut handled, group, committed);
            continue;
        }
        if waiting.contains(&entry.seq) {
            continue;
        }
        if let Some(block) = check(entry, committed) {
            if let FlushBlock::OpenIntent { .. } = block
                && entry.op.batch().is_none()
            {
                // Its paths take no other record until it settles, so later
                // records commit ahead of it; it is committed (or dropped)
                // once recovery settles it.
                out.blocked.get_or_insert(block);
                continue;
            }
            out.blocked = Some(block);
            break;
        }
        if matches!(entry.status, OpStatus::Aborted | OpStatus::Conflict { .. }) {
            handled.insert(entry.seq);
            continue;
        }
        let Some(batch) = entry.op.batch() else {
            emit(
                &mut out,
                &mut handled,
                Group {
                    entries: vec![entry],
                    batch: None,
                    stale_upserts: BTreeSet::new(),
                    stale_removals: BTreeSet::new(),
                },
                committed,
            );
            continue;
        };
        match batch_group(&entries[idx..], &batch.id, committed) {
            Err(block) => {
                out.blocked = Some(block);
                break;
            }
            Ok((group, last)) if last == entry.seq => {
                emit(&mut out, &mut handled, group, committed);
            }
            Ok((group, last)) => {
                waiting.extend(group.entries.iter().map(|e| e.seq).filter(|&s| s != last));
                pending.insert(last, group);
            }
        }
    }
    // Retire only the prefix whose every record is committed or settled.
    for entry in entries {
        if !handled.contains(&entry.seq) {
            break;
        }
        out.retire_upto = Some(entry.seq);
    }
    out
}

fn check(entry: &ManifestEntry, committed: &Committed) -> Option<FlushBlock> {
    if let Some(commit) = committed.mismatched.get(&entry.seq) {
        return Some(FlushBlock::CommittedMismatch {
            seq: entry.seq,
            commit: commit.to_string(),
        });
    }
    status_block(entry)
}

/// Members of the batch starting at `rest[0]`, through its first `last`
/// record, and that record's sequence id.
fn batch_group<'a>(
    rest: &'a [ManifestEntry],
    id: &str,
    committed: &Committed,
) -> std::result::Result<(Group<'a>, Seq), FlushBlock> {
    let mut members = Vec::new();
    let mut interleaved = Vec::new();
    let mut last = None;
    for e in rest {
        if e.op.batch().is_some_and(|b| b.id == id) {
            members.push(e);
            if e.op.batch().is_some_and(|b| b.last) {
                last = Some(e.seq);
                break;
            }
        } else {
            interleaved.push(e);
        }
    }
    let Some(last) = last else {
        return Err(FlushBlock::IncompleteBatch {
            seq: rest[0].seq,
            batch: id.to_owned(),
        });
    };
    if let Some(block) = members.iter().find_map(|e| check(e, committed)) {
        return Err(block);
    }
    // An interleaved record commits before this batch unless it belongs to
    // another batch whose `last` comes after this one; such a record commits
    // later and overwrites this batch's effects on its own.
    let closed: BTreeSet<&str> = interleaved
        .iter()
        .filter_map(|x| x.op.batch().filter(|b| b.last).map(|b| b.id.as_str()))
        .collect();
    let earlier: Vec<&ManifestEntry> = interleaved
        .into_iter()
        .filter(|x| x.status == OpStatus::Done)
        .filter(|x| x.op.batch().is_none_or(|b| closed.contains(b.id.as_str())))
        .collect();
    let newer = |m: &ManifestEntry, path: &str| {
        earlier
            .iter()
            .any(|x| x.seq > m.seq && (x.op.path() == path || x.op.moved_from() == Some(path)))
    };
    let stale_upserts = members
        .iter()
        .filter(|m| newer(m, m.op.path()))
        .map(|m| m.seq)
        .collect();
    let stale_removals = members
        .iter()
        .filter(|m| m.op.moved_from().is_some_and(|from| newer(m, from)))
        .map(|m| m.seq)
        .collect();
    Ok((
        Group {
            entries: members,
            batch: Some(id.to_owned()),
            stale_upserts,
            stale_removals,
        },
        last,
    ))
}

/// Add a group's records to the plan: committed ones become `already`,
/// settled non-commits are only retired, the rest become one commit.
fn emit<'a>(
    out: &mut Plan<'a>,
    handled: &mut BTreeSet<Seq>,
    group: Group<'a>,
    committed: &Committed,
) {
    handled.extend(group.entries.iter().map(|e| e.seq));
    let (done, todo): (Vec<_>, Vec<_>) = group
        .entries
        .into_iter()
        .filter(|e| e.status == OpStatus::Done)
        .partition(|e| committed.verified.contains(&e.seq));
    out.already.extend(done.iter().map(|e| e.seq));
    if !todo.is_empty() {
        out.groups.push(Group {
            entries: todo,
            batch: group.batch,
            stale_upserts: group.stale_upserts,
            stale_removals: group.stale_removals,
        });
    }
}

/// Split the sequence ids named by commits into those whose commit holds
/// the record and those it does not. A commit holds a record when its
/// `Wi-Actor` (or `Wi-Batch` for a batch) matches and each of the record's
/// path effects is visible in its tree or was superseded by a later record
/// (see [`effect_holds`]). A mismatch means a sequence id was reused and
/// blocks the flush instead of silently retiring a different operation.
fn verify_committed(
    vault: &Vault,
    snap: &ManifestSnapshot,
    named: &BTreeMap<Seq, ObjectId>,
) -> Result<Committed> {
    let mut out = Committed {
        verified: BTreeSet::new(),
        mismatched: BTreeMap::new(),
    };
    let mut messages: BTreeMap<ObjectId, Vec<(String, String)>> = BTreeMap::new();
    for entry in &snap.entries {
        let Some(&commit) = named.get(&entry.seq) else {
            continue;
        };
        let trailers = match messages.entry(commit) {
            std::collections::btree_map::Entry::Occupied(found) => found.into_mut(),
            std::collections::btree_map::Entry::Vacant(slot) => {
                let (message, _) = gitops::commit_message(vault, commit)?;
                slot.insert(gitops::trailers(&message))
            }
        };
        let trailer = |key: &str| {
            trailers
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let who = match entry.op.batch() {
            Some(b) => trailer(crate::TRAILER_BATCH) == Some(b.id.as_str()),
            None => trailer(crate::TRAILER_ACTOR) == Some(entry.op.actor()),
        };
        let upsert = effect_holds(
            snap,
            entry,
            entry.op.path(),
            Some(entry.op.new_blob()),
            gitops::blob_at(vault, commit, entry.op.path())?,
        );
        let removal = match entry.op.moved_from() {
            Some(from) => effect_holds(
                snap,
                entry,
                from,
                None,
                gitops::blob_at(vault, commit, from)?,
            ),
            None => true,
        };
        if entry.status == OpStatus::Done && who && upsert && removal {
            out.verified.insert(entry.seq);
        } else {
            out.mismatched.insert(entry.seq, commit);
        }
    }
    Ok(out)
}

/// Whether a commit's state `at` of `path` agrees with `entry`'s effect on
/// it (`blob` written, or `None` for a move's source removal). The state may
/// also be any later record's effect on the same path, because a batch
/// commit skips member effects that a newer interleaved record superseded,
/// and within one commit later members overwrite earlier ones.
fn effect_holds(
    snap: &ManifestSnapshot,
    entry: &ManifestEntry,
    path: &str,
    blob: Option<&crate::BlobId>,
    at: Option<ObjectId>,
) -> bool {
    let same = |want: Option<&crate::BlobId>| match (want, at) {
        (Some(want), Some(at)) => want.oid().is_ok_and(|oid| oid == at),
        (None, None) => true,
        _ => false,
    };
    same(blob)
        || snap.entries.iter().any(|e| {
            e.seq > entry.seq
                && ((e.op.path() == path && same(Some(e.op.new_blob())))
                    || (e.op.moved_from() == Some(path) && same(None)))
        })
}

/// Compress ascending sequence ids into `a`, `a..b` runs joined by commas.
pub(crate) fn format_seqs(seqs: &[Seq]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < seqs.len() {
        let mut j = i;
        while j + 1 < seqs.len() && seqs[j + 1] == seqs[j] + 1 {
            j += 1;
        }
        parts.push(if i == j {
            seqs[i].to_string()
        } else {
            format!("{}..{}", seqs[i], seqs[j])
        });
        i = j + 1;
    }
    parts.join(",")
}

/// Distinct paths a group touches: a path written twice (for example a run
/// note rewritten after its sidecar) is one file, and a move touches two.
fn touched_paths(entries: &[&ManifestEntry]) -> usize {
    entries
        .iter()
        .flat_map(|e| std::iter::once(e.op.path()).chain(e.op.moved_from()))
        .collect::<BTreeSet<_>>()
        .len()
}

fn message(group: &Group<'_>, manifest_id: &str) -> String {
    let seqs: Vec<Seq> = group.entries.iter().map(|e| e.seq).collect();
    let first = &group.entries[0].op;
    let subject = match &group.batch {
        Some(id) => format!(
            "wi: {} batch {id} ({} files)",
            first.actor(),
            touched_paths(&group.entries)
        ),
        None => format!("wi: {} {}", first.actor(), first.path()),
    };
    let mut lines = vec![
        subject,
        String::new(),
        format!(
            "{}: {} {manifest_id}",
            crate::TRAILER_SEQ,
            format_seqs(&seqs)
        ),
        format!("{}: {}", crate::TRAILER_ACTOR, first.actor()),
    ];
    if let Some(id) = &group.batch {
        lines.push(format!("{}: {id}", crate::TRAILER_BATCH));
    }
    for entry in &group.entries {
        if let OpRecord::Intent(rec) = &entry.op {
            if let Some(op_id) = &rec.op_id {
                lines.push(format!(
                    "{}: {op_id} {}",
                    crate::TRAILER_OPERATION,
                    rec.request_digest
                ));
            }
            if let Some(from) = &rec.moved_from {
                lines.push(format!("{}: {from}", crate::TRAILER_MOVED_FROM));
            }
            if let Some(commit) = &rec.revert_of {
                lines.push(format!("{}: {commit}", crate::TRAILER_REVERT));
            }
            for loss in &rec.losses {
                lines.push(format!("{}: {loss}", crate::TRAILER_LOSS));
            }
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Commits manifest records and retires the committed prefix.
pub struct Flusher;

impl Flusher {
    /// Flush under the flush lock. Saves may append concurrently; records
    /// appended after the snapshot stay in the manifest for the next flush.
    pub fn flush(vault: &Vault) -> Result<FlushReport> {
        let lock = FlushLock::acquire(vault)?;
        if vault.wi_dir().join("sync-intent.json").exists() {
            return Err(HistoryError::Git(
                "sync recovery is finishing; retry the flush".into(),
            ));
        }
        Self::flush_locked(vault, &lock)
    }

    pub(crate) fn flush_locked(vault: &Vault, _lock: &FlushLock) -> Result<FlushReport> {
        let snap = {
            let guard = PublisherLock::acquire(vault)?;
            Manifest::snapshot(vault, &guard)?
        };
        fault::pause_after_snapshot();
        let (branch, tip) = gitops::branch_tip(vault)?;
        let named = if snap.entries.is_empty() {
            BTreeMap::new()
        } else {
            gitops::committed_seqs(vault, tip, &snap.manifest_id, snap.retired_upto)?
        };
        let committed = verify_committed(vault, &snap, &named)?;
        let plan = plan(&snap, &committed);
        let mut report = FlushReport {
            already_committed: plan.already.clone(),
            blocked: plan.blocked.clone(),
            retired_upto: snap.retired_upto,
            ..FlushReport::default()
        };
        let mut parent = tip;
        for group in &plan.groups {
            let (commit, seqs) = commit_group(vault, parent, group, &snap.manifest_id)?;
            report.commits.push(CommitSummary {
                commit: commit.to_string(),
                seqs,
            });
            parent = Some(commit);
        }
        if let (Some(new), true) = (parent, parent != tip) {
            fault::pause_before_advance();
            let last = report
                .commits
                .last()
                .map(|c| format_seqs(&c.seqs))
                .unwrap_or_default();
            gitops::advance_branch(vault, &branch, tip, new, &format!("wi: flush {last}"))?;
            fault::after_commit_before_retire();
            report.index_warning = gitops::sync_git_index(vault, new)
                .err()
                .map(|e| e.to_string());
        }
        if report.index_warning.is_none()
            && vault.wi_dir().join("git-index-pending").exists()
            && let Some(tip) = parent
        {
            report.index_warning = gitops::sync_git_index(vault, tip)
                .err()
                .map(|error| error.to_string());
        }
        if !plan.already.is_empty() {
            // Found committed by an earlier flush that may have died (or
            // failed an fsync) between publishing and its barriers: barrier
            // those commits and the ref again before their records go.
            barrier_already(vault, &snap, &named, &plan.already)?;
            gitops::barrier_ref(vault, &branch)?;
        }
        if let Some(upto) = plan.retire_upto {
            let guard = PublisherLock::acquire(vault)?;
            Manifest::retire(vault, &guard, upto)?;
            report.retired_upto = upto.max(snap.retired_upto);
            report.remaining = Manifest::snapshot(vault, &guard)?.entries.len();
        } else {
            report.remaining = snap.entries.len();
        }
        Ok(report)
    }
}

/// Write one group's commit on `parent` and barrier its objects, including
/// the trees above every path it removed (a move's source directory).
fn commit_group(
    vault: &Vault,
    parent: Option<ObjectId>,
    group: &Group<'_>,
    manifest_id: &str,
) -> Result<(ObjectId, Vec<Seq>)> {
    let seqs: Vec<Seq> = group.entries.iter().map(|e| e.seq).collect();
    let mut edits = Vec::new();
    let mut paths: Vec<&str> = Vec::new();
    for entry in &group.entries {
        if let Some(from) = entry.op.moved_from()
            && !group.stale_removals.contains(&entry.seq)
        {
            edits.push(TreeEdit::Remove { path: from });
            paths.push(from);
        }
        if group.stale_upserts.contains(&entry.seq) {
            continue;
        }
        let blob = entry.op.new_blob().oid()?;
        if !vault.has_object(&blob) {
            return Err(HistoryError::MissingBlob(entry.op.new_blob().0.clone()));
        }
        edits.push(TreeEdit::Upsert {
            path: entry.op.path(),
            blob,
        });
        paths.push(entry.op.path());
    }
    let author_secs = i64::try_from(group.entries[0].time_ms / 1000).unwrap_or(0);
    let (commit, _) = gitops::write_commit(
        vault,
        parent,
        &edits,
        &message(group, manifest_id),
        author_secs,
    )?;
    gitops::sync_commit(vault, commit, &paths)?;
    Ok((commit, seqs))
}

/// Barrier the commits that hold already-committed records, along the
/// records' paths (and moved-from paths).
fn barrier_already(
    vault: &Vault,
    snap: &ManifestSnapshot,
    named: &BTreeMap<Seq, ObjectId>,
    already: &[Seq],
) -> Result<()> {
    let mut by_commit: BTreeMap<ObjectId, Vec<&str>> = BTreeMap::new();
    for seq in already {
        let (Some(&commit), Some(entry)) = (named.get(seq), snap.entry(*seq)) else {
            continue;
        };
        let paths = by_commit.entry(commit).or_default();
        paths.push(entry.op.path());
        paths.extend(entry.op.moved_from());
    }
    for (commit, paths) in &by_commit {
        gitops::sync_commit(vault, *commit, paths)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::format_seqs;

    #[test]
    fn seq_lists_compress_runs() {
        assert_eq!(format_seqs(&[4]), "4");
        assert_eq!(format_seqs(&[2, 3, 4]), "2..4");
        assert_eq!(format_seqs(&[2, 3, 5, 7, 8]), "2..3,5,7..8");
    }
}
