//! Thin git layer over gix: commits on the current branch with
//! compare-and-swap ref updates, trailers and durability. Nothing here amends,
//! resets or force-updates a ref.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};

use crate::durable;
use crate::error::{HistoryError, Result};
use crate::fault;
use crate::manifest::Seq;
use crate::vault::Vault;

const ENGINE_NAME: &str = "wiki-island";
const ENGINE_EMAIL: &str = "wiki-island@localhost";

#[cfg(windows)]
fn object_repository(vault: &Vault) -> Result<gix::Repository> {
    let mut repo = vault.repo().clone();
    // Objects and the staging index must retain remote names that this
    // device cannot materialize. The store still validates every disk path;
    // keep gix's separate NTFS/.git protections enabled.
    let mut config = repo.config_snapshot_mut();
    config
        .set_value(&gix::config::tree::gitoxide::Core::PROTECT_WINDOWS, "false")
        .map_err(HistoryError::git)?;
    config.commit().map_err(HistoryError::git)?;
    Ok(repo)
}

/// A tree change applied by a commit.
pub(crate) enum TreeEdit<'a> {
    Upsert { path: &'a str, blob: ObjectId },
    Remove { path: &'a str },
}

/// The branch HEAD points at and its current tip (`None` when unborn).
pub(crate) fn branch_tip(vault: &Vault) -> Result<(FullName, Option<ObjectId>)> {
    let repo = vault.repo();
    let name = repo
        .head_name()
        .map_err(HistoryError::git)?
        .ok_or(HistoryError::DetachedHead)?;
    let tip = repo
        .try_find_reference(name.as_ref())
        .map_err(HistoryError::git)?
        .and_then(|r| r.target().try_id().map(ToOwned::to_owned));
    Ok((name, tip))
}

fn identity(vault: &Vault) -> (String, String) {
    match vault.repo().committer() {
        Some(Ok(sig)) => (sig.name.to_string(), sig.email.to_string()),
        _ => (ENGINE_NAME.to_owned(), ENGINE_EMAIL.to_owned()),
    }
}

fn signature(vault: &Vault, seconds: i64) -> gix::actor::Signature {
    let (name, email) = identity(vault);
    gix::actor::Signature {
        name: name.into(),
        email: email.into(),
        time: gix::date::Time { seconds, offset: 0 },
    }
}

pub(crate) fn now_secs() -> i64 {
    i64::try_from(crate::manifest::now_ms() / 1000).unwrap_or(i64::MAX)
}

pub(crate) fn tree_of(vault: &Vault, commit: Option<ObjectId>) -> Result<ObjectId> {
    let repo = vault.repo();
    match commit {
        None => Ok(ObjectId::empty_tree(repo.object_hash())),
        Some(id) => Ok(repo
            .find_commit(id)
            .map_err(HistoryError::git)?
            .tree_id()
            .map_err(HistoryError::git)?
            .detach()),
    }
}

/// Write a commit object (not moving any ref) and return `(commit, tree)`.
pub(crate) fn write_commit(
    vault: &Vault,
    parent: Option<ObjectId>,
    edits: &[TreeEdit<'_>],
    message: &str,
    author_secs: i64,
) -> Result<(ObjectId, ObjectId)> {
    #[cfg(not(windows))]
    let repo = vault.repo();
    #[cfg(windows)]
    let repo = object_repository(vault)?;
    let mut editor = repo
        .edit_tree(tree_of(vault, parent)?)
        .map_err(HistoryError::git)?;
    for edit in edits {
        match edit {
            TreeEdit::Upsert { path, blob } => {
                editor
                    .upsert(*path, gix::object::tree::EntryKind::Blob, *blob)
                    .map_err(HistoryError::git)?;
            }
            TreeEdit::Remove { path } => {
                editor.remove(*path).map_err(HistoryError::git)?;
            }
        }
    }
    let tree = editor.write().map_err(HistoryError::git)?.detach();
    let commit = gix::objs::Commit {
        message: message.into(),
        tree,
        author: signature(vault, author_secs),
        committer: signature(vault, now_secs()),
        encoding: None,
        parents: parent.into_iter().collect(),
        extra_headers: Vec::new(),
    };
    let id = repo
        .write_object(commit)
        .map_err(HistoryError::git)?
        .detach();
    Ok((id, tree))
}

/// Barrier a commit, its root tree and every tree and blob along `paths`.
/// A path the commit removed still barriers the surviving trees above it.
pub(crate) fn sync_commit(vault: &Vault, commit: ObjectId, paths: &[&str]) -> Result<()> {
    let repo = vault.repo();
    vault.sync_object(&commit)?;
    let root = tree_of(vault, Some(commit))?;
    vault.sync_object(&root)?;
    for path in paths {
        let mut tree = root;
        let parts: Vec<&str> = path.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            let decoded = repo.find_tree(tree).map_err(HistoryError::git)?;
            let Some(entry) = decoded
                .decode()
                .map_err(HistoryError::git)?
                .entries
                .iter()
                .find(|e| e.filename == part.as_bytes())
                .map(|e| (e.oid.to_owned(), e.mode.is_tree()))
            else {
                break;
            };
            vault.sync_object(&entry.0)?;
            if i + 1 < parts.len() && entry.1 {
                tree = entry.0;
            } else {
                break;
            }
        }
    }
    Ok(())
}

/// Move the branch from `expected` to `new` with compare-and-swap, then
/// barrier the ref ([`barrier_ref`]). A moved branch fails with `RefMoved`;
/// nothing is forced.
pub(crate) fn advance_branch(
    vault: &Vault,
    name: &FullName,
    expected: Option<ObjectId>,
    new: ObjectId,
    log_message: &str,
) -> Result<()> {
    let repo = vault.repo();
    let previous = match expected {
        Some(id) => PreviousValue::MustExistAndMatch(Target::Object(id)),
        None => PreviousValue::MustNotExist,
    };
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: log_message.into(),
            },
            expected: previous,
            new: Target::Object(new),
        },
        name: name.clone(),
        deref: false,
    };
    let sig = signature(vault, now_secs());
    let mut buf = gix::date::parse::TimeBuf::default();
    repo.edit_references_as(Some(edit), Some(sig.to_ref(&mut buf)))
        .map_err(|_| HistoryError::RefMoved {
            expected: expected.map_or_else(|| "unborn".to_owned(), |id| id.to_string()),
        })?;
    fault::after_ref_before_barrier()?;
    barrier_ref(vault, name)
}

/// Barrier the branch ref: the loose ref file and every directory from its
/// parent up to `.git/refs` (gix may have created several levels, as for
/// `refs/heads/a/b`), or `packed-refs` when the ref is not loose. A flush
/// runs this before every retirement, including when it wrote no commit,
/// because a visible ref is not proof that an earlier barrier completed.
pub(crate) fn barrier_ref(vault: &Vault, name: &FullName) -> Result<()> {
    let mode = vault.durability();
    let git_dir = vault.git_dir();
    let ref_path: PathBuf = git_dir.join(name.as_bstr().to_str_lossy().as_ref());
    if !ref_path.is_file() {
        let packed = git_dir.join("packed-refs");
        if packed.is_file() {
            durable::barrier_path(&packed, mode)?;
        }
        return durable::barrier_dir(git_dir, mode);
    }
    durable::barrier_path(&ref_path, mode)?;
    let refs_root = git_dir.join("refs");
    let mut dir = ref_path.parent();
    while let Some(d) = dir {
        if !d.starts_with(&refs_root) {
            break;
        }
        durable::barrier_dir(d, mode)?;
        dir = d.parent();
    }
    Ok(())
}

/// `Wi-*` trailer lines of a commit message, in order.
pub(crate) fn trailers(message: &str) -> Vec<(String, String)> {
    message
        .lines()
        .filter(|l| l.starts_with("Wi-"))
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_owned(), v.trim().to_owned()))
        .collect()
}

/// Parse `Wi-Seq: <list> <manifest-id>`, where the list is comma-separated
/// `a` or `a..b` runs.
pub(crate) fn parse_seq_trailer(value: &str) -> Option<(Vec<Seq>, &str)> {
    let (list, id) = value.split_once(' ')?;
    let mut seqs = Vec::new();
    for part in list.split(',') {
        match part.split_once("..") {
            Some((a, b)) => {
                let (a, b): (Seq, Seq) = (a.parse().ok()?, b.parse().ok()?);
                if a > b {
                    return None;
                }
                seqs.extend(a..=b);
            }
            None => seqs.push(part.parse().ok()?),
        }
    }
    Some((seqs, id))
}

/// Author time (unix seconds) of a commit.
pub(crate) fn commit_time(vault: &Vault, id: ObjectId) -> Result<i64> {
    let commit = vault.repo().find_commit(id).map_err(HistoryError::git)?;
    Ok(commit
        .author()
        .map_err(HistoryError::git)?
        .time()
        .map(|t| t.seconds)
        .unwrap_or(0))
}

pub(crate) fn commit_message(vault: &Vault, id: ObjectId) -> Result<(String, Vec<ObjectId>)> {
    let commit = vault.repo().find_commit(id).map_err(HistoryError::git)?;
    let message = commit
        .message_raw()
        .map_err(HistoryError::git)?
        .to_str_lossy()
        .into_owned();
    let parents = commit.parent_ids().map(|p| p.detach()).collect();
    Ok((message, parents))
}

/// Sequence ids of `manifest_id` already committed on the first-parent chain
/// from `tip`, each with the commit naming it. The walk stops at the first
/// commit naming a sequence id at or below `retired_upto`.
pub(crate) fn committed_seqs(
    vault: &Vault,
    tip: Option<ObjectId>,
    manifest_id: &str,
    retired_upto: Seq,
) -> Result<BTreeMap<Seq, ObjectId>> {
    let mut found = BTreeMap::new();
    let mut cursor = tip;
    while let Some(id) = cursor {
        let (message, parents) = commit_message(vault, id)?;
        let mut stop = false;
        for (key, value) in trailers(&message) {
            if key != crate::TRAILER_SEQ {
                continue;
            }
            if let Some((seqs, mid)) = parse_seq_trailer(&value)
                && mid == manifest_id
            {
                stop |= seqs.iter().any(|&s| s <= retired_upto);
                for seq in seqs {
                    // Keep the newest commit naming a sequence id.
                    found.entry(seq).or_insert(id);
                }
            }
        }
        if stop {
            break;
        }
        cursor = parents.first().copied();
    }
    Ok(found)
}

/// Blob id at `path` in `commit`, if present.
pub(crate) fn blob_at(vault: &Vault, commit: ObjectId, path: &str) -> Result<Option<ObjectId>> {
    let repo = vault.repo();
    let tree = repo
        .find_commit(commit)
        .map_err(HistoryError::git)?
        .tree()
        .map_err(HistoryError::git)?;
    let entry = tree.lookup_entry_by_path(path).map_err(HistoryError::git)?;
    Ok(entry.filter(|e| e.mode().is_blob()).map(|e| e.object_id()))
}

/// Every blob path in the tree of `commit`.
pub(crate) fn tree_blobs(vault: &Vault, commit: ObjectId) -> Result<Vec<(String, ObjectId)>> {
    let repo = vault.repo();
    let mut out = Vec::new();
    let mut stack = vec![(String::new(), tree_of(vault, Some(commit))?)];
    while let Some((prefix, id)) = stack.pop() {
        let tree = repo.find_tree(id).map_err(HistoryError::git)?;
        for entry in tree.decode().map_err(HistoryError::git)?.entries {
            let name = format!("{prefix}{}", entry.filename.to_str_lossy());
            if entry.mode.is_tree() {
                stack.push((format!("{name}/"), entry.oid.to_owned()));
            } else if entry.mode.is_blob() {
                out.push((name, entry.oid.to_owned()));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Rewrite `.git/index` from the tree of `commit` so that git tools see the
/// engine's commits as the staged state (a stale index would make a manual
/// `git commit` drop engine files). Stat data is empty, so git re-hashes on
/// the next `git status`. Fails without waiting if `index.lock` is held.
pub(crate) fn sync_git_index(vault: &Vault, commit: ObjectId) -> Result<()> {
    let pending = vault.wi_dir().join("git-index-pending");
    let result: Result<()> = (|| {
        let tree = tree_of(vault, Some(commit))?;
        #[cfg(not(windows))]
        let repo = vault.repo();
        #[cfg(windows)]
        let repo = object_repository(vault)?;
        let mut index = repo.index_from_tree(&tree).map_err(HistoryError::git)?;
        index
            .write(gix::index::write::Options::default())
            .map_err(HistoryError::git)
    })();
    if result.is_err() {
        durable::write_atomic(
            &pending,
            b"refresh the staging index from the current branch\n",
            vault.durability(),
        )?;
    } else {
        match std::fs::remove_file(&pending) {
            Ok(()) => durable::barrier_dir(&vault.wi_dir(), vault.durability())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(HistoryError::io(&pending, error)),
        }
    }
    result
}
