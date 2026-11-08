//! Paths published after a point in the publication sequence. A read uses
//! this to tell a publication that touched what it returned from an
//! unrelated one (an export regeneration, another note).

use std::collections::BTreeSet;

use gix::ObjectId;
use gix::bstr::ByteSlice;

use crate::error::{HistoryError, Result};
use crate::gitops;
use crate::manifest::{Manifest, OpRecord, OpStatus, Seq};
use crate::vault::Vault;

/// First-parent commits inspected for records retired during a read.
const RETIRED_WALK_LIMIT: usize = 64;

/// The publication sequence at one instant, read without a lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationMark {
    manifest_id: String,
    generation: Seq,
    next_seq: Seq,
    open: Option<Seq>,
}

impl PublicationMark {
    /// The vault generation at the mark.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Sequence id of an intent open at the mark.
    pub fn open(&self) -> Option<u64> {
        self.open
    }
}

/// The current publication mark.
pub fn publication_mark(vault: &Vault) -> Result<PublicationMark> {
    let snap = Manifest::snapshot_unlocked(vault)?;
    Ok(PublicationMark {
        open: if vault.wi_dir().join("sync-intent.json").exists() {
            Some(u64::MAX)
        } else {
            snap.open_intent().map(|e| e.seq)
        },
        manifest_id: snap.manifest_id,
        generation: snap.generation,
        next_seq: snap.next_seq,
    })
}

/// Vault-relative paths written, moved or opened by records after `mark`
/// (including an intent open at the mark or now). `None` when they cannot
/// be determined, such as a recreated manifest: treat every path as changed.
pub fn changed_paths_since(
    vault: &Vault,
    mark: &PublicationMark,
) -> Result<Option<BTreeSet<String>>> {
    let snap = Manifest::snapshot_unlocked(vault)?;
    if mark.open == Some(u64::MAX) || vault.wi_dir().join("sync-intent.json").exists() {
        return Ok(None);
    }
    if snap.manifest_id != mark.manifest_id {
        return Ok(None);
    }
    let lower = mark.open.unwrap_or(mark.next_seq).min(mark.next_seq);
    let mut paths = BTreeSet::new();
    for entry in &snap.entries {
        if entry.seq < lower && entry.status != OpStatus::Open {
            continue;
        }
        paths.insert(entry.op.path().to_owned());
        if let OpRecord::Intent(rec) = &entry.op
            && let Some(from) = &rec.moved_from
        {
            paths.insert(from.clone());
        }
    }
    if snap.retired_upto >= lower {
        // Records committed and retired during the read are only in git.
        match retired_paths(vault, &snap.manifest_id, lower, snap.retired_upto)? {
            Some(retired) => paths.extend(retired),
            None => return Ok(None),
        }
    }
    Ok(Some(paths))
}

/// Paths changed by first-parent commits naming sequence ids in
/// `lower..=upper` of `manifest_id`; `None` past the walk limit.
fn retired_paths(
    vault: &Vault,
    manifest_id: &str,
    lower: Seq,
    upper: Seq,
) -> Result<Option<BTreeSet<String>>> {
    let mut paths = BTreeSet::new();
    let mut cursor = gitops::branch_tip(vault)?.1;
    for _ in 0..RETIRED_WALK_LIMIT {
        let Some(id) = cursor else {
            return Ok(Some(paths));
        };
        let (message, parents) = gitops::commit_message(vault, id)?;
        let seqs: Vec<Seq> = gitops::trailers(&message)
            .into_iter()
            .filter(|(k, _)| k == crate::TRAILER_SEQ)
            .filter_map(|(_, v)| {
                gitops::parse_seq_trailer(&v)
                    .filter(|(_, mid)| *mid == manifest_id)
                    .map(|(s, _)| s)
            })
            .flatten()
            .collect();
        if !seqs.is_empty() && seqs.iter().all(|&s| s < lower) {
            return Ok(Some(paths));
        }
        if seqs.iter().any(|s| (lower..=upper).contains(s)) {
            paths.extend(commit_changes(vault, id, parents.first().copied())?);
        }
        cursor = parents.first().copied();
    }
    Ok(None)
}

/// Blob paths that differ between `commit` and `parent` (added, removed
/// or changed), descending only into subtrees whose ids differ.
fn commit_changes(
    vault: &Vault,
    commit: ObjectId,
    parent: Option<ObjectId>,
) -> Result<Vec<String>> {
    let repo = vault.repo();
    let tree = gitops::tree_of(vault, Some(commit))?;
    let base = match parent {
        Some(p) => Some(gitops::tree_of(vault, Some(p))?),
        None => None,
    };
    let mut out = Vec::new();
    let mut stack = vec![(String::new(), Some(tree), base)];
    while let Some((prefix, new, old)) = stack.pop() {
        if new == old {
            continue;
        }
        let entries = |id: Option<ObjectId>| -> Result<Vec<(String, bool, ObjectId)>> {
            let Some(id) = id else {
                return Ok(Vec::new());
            };
            let tree = repo.find_tree(id).map_err(HistoryError::git)?;
            Ok(tree
                .decode()
                .map_err(HistoryError::git)?
                .entries
                .iter()
                .map(|e| {
                    (
                        e.filename.to_str_lossy().into_owned(),
                        e.mode.is_tree(),
                        e.oid.to_owned(),
                    )
                })
                .collect())
        };
        let new_entries = entries(new)?;
        let old_entries = entries(old)?;
        let find = |list: &[(String, bool, ObjectId)], name: &str, tree: bool| {
            list.iter()
                .find(|(n, t, _)| n == name && *t == tree)
                .map(|(_, _, id)| *id)
        };
        let names: BTreeSet<(&str, bool)> = new_entries
            .iter()
            .chain(&old_entries)
            .map(|(n, t, _)| (n.as_str(), *t))
            .collect();
        for (name, is_tree) in names {
            let a = find(&new_entries, name, is_tree);
            let b = find(&old_entries, name, is_tree);
            if a == b {
                continue;
            }
            let path = format!("{prefix}{name}");
            if is_tree {
                stack.push((format!("{path}/"), a, b));
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}
