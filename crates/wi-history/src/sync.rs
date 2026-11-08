//! Local git primitives for remote synchronization. Transport is owned by
//! the store; this module never invokes a process or accesses the network.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use gix::ObjectId;

use crate::error::{HistoryError, Result};
use crate::{BlobId, PublisherGuard, Vault, gitops};

/// Blob-only tree snapshot. Symlinks and submodules are refused.
pub type SyncTree = BTreeMap<String, BlobId>;

fn oid(value: &str) -> Result<ObjectId> {
    ObjectId::from_hex(value.as_bytes()).map_err(HistoryError::git)
}

/// Operations on git objects and refs; the caller holds flush then publisher
/// locks throughout planning, working-tree application and publication.
pub struct SyncRepo;
impl SyncRepo {
    pub fn head(vault: &Vault) -> Result<Option<String>> {
        Ok(gitops::branch_tip(vault)?.1.map(|v| v.to_string()))
    }

    pub fn tree(vault: &Vault, commit: Option<&str>) -> Result<SyncTree> {
        let Some(commit) = commit else {
            return Ok(BTreeMap::new());
        };
        let id = oid(commit)?;
        // Validate modes and names before any working-tree operation.
        let mut stack = vec![(String::new(), gitops::tree_of(vault, Some(id))?)];
        let mut out = BTreeMap::new();
        while let Some((prefix, tree)) = stack.pop() {
            let tree = vault.repo().find_tree(tree).map_err(HistoryError::git)?;
            for entry in tree.decode().map_err(HistoryError::git)?.entries {
                let name = std::str::from_utf8(entry.filename).map_err(HistoryError::git)?;
                if name.is_empty()
                    || name == "."
                    || name == ".."
                    || name.contains(['/', '\\'])
                    || name.chars().any(char::is_control)
                {
                    return Err(HistoryError::Git(
                        "remote tree contains an unsupported path".into(),
                    ));
                }
                let path = format!("{prefix}{name}");
                if entry.mode.is_tree() {
                    stack.push((format!("{path}/"), entry.oid.to_owned()));
                } else if entry.mode.kind() == gix::object::tree::EntryKind::Blob
                    || entry.mode.kind() == gix::object::tree::EntryKind::BlobExecutable
                {
                    out.insert(path, BlobId(entry.oid.to_string()));
                } else {
                    return Err(HistoryError::Git(
                        "remote tree contains a symlink or submodule".into(),
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Ancestor distances, bounded to avoid unbounded memory on malformed
    /// or unexpectedly large histories.
    fn ancestors(vault: &Vault, start: &str) -> Result<BTreeMap<String, usize>> {
        let mut seen = BTreeMap::new();
        let mut queue = VecDeque::from([(oid(start)?, 0)]);
        while let Some((id, depth)) = queue.pop_front() {
            if seen.contains_key(&id.to_string()) {
                continue;
            }
            if seen.len() >= 100_000 {
                return Err(HistoryError::Git(
                    "sync history exceeds the traversal budget".into(),
                ));
            }
            seen.insert(id.to_string(), depth);
            for parent in gitops::commit_message(vault, id)?.1 {
                queue.push_back((parent, depth + 1));
            }
        }
        Ok(seen)
    }

    pub fn merge_base(vault: &Vault, local: &str, remote: &str) -> Result<Option<String>> {
        let a = Self::ancestors(vault, local)?;
        let b = Self::ancestors(vault, remote)?;
        let common: BTreeSet<_> = a.keys().filter(|id| b.contains_key(*id)).cloned().collect();
        let mut best = common.clone();
        // Common ancestors are closed under parent traversal. A common
        // commit that is a parent of another common commit cannot be a
        // best base, regardless of graph shortcuts or commit timestamps.
        for id in &common {
            for parent in gitops::commit_message(vault, oid(id)?)?.1 {
                best.remove(&parent.to_string());
            }
        }
        // Multiple best bases use an empty base. That conservatively
        // preserves both differing versions rather than selecting a claim.
        Ok((best.len() == 1).then(|| best.into_iter().next()).flatten())
    }

    /// Write and barrier an immutable merge object with local first parent.
    pub fn merge(
        vault: &Vault,
        guard: &PublisherGuard,
        local: &str,
        remote: &str,
        tree: &SyncTree,
    ) -> Result<String> {
        guard.check(vault)?;
        let before = Self::tree(vault, Some(local))?;
        let paths: BTreeSet<_> = before.keys().chain(tree.keys()).collect();
        let edits: Vec<_> = paths
            .iter()
            .map(|p| match tree.get(*p) {
                Some(blob) => Ok(gitops::TreeEdit::Upsert {
                    path: p.as_str(),
                    blob: oid(&blob.0)?,
                }),
                None => Ok(gitops::TreeEdit::Remove { path: p.as_str() }),
            })
            .collect::<Result<_>>()?;
        let (_, tree_id) = gitops::write_commit(
            vault,
            Some(oid(local)?),
            &edits,
            "synchronize notes\n\nWi-Actor: sync\n",
            gitops::now_secs(),
        )?;
        let commit = gix::objs::Commit {
            message: "synchronize notes\n\nWi-Actor: sync\n".into(),
            tree: tree_id,
            author: gix::actor::Signature {
                name: "wiki-island".into(),
                email: "wiki-island@localhost".into(),
                time: gix::date::Time {
                    seconds: gitops::now_secs(),
                    offset: 0,
                },
            },
            committer: gix::actor::Signature {
                name: "wiki-island".into(),
                email: "wiki-island@localhost".into(),
                time: gix::date::Time {
                    seconds: gitops::now_secs(),
                    offset: 0,
                },
            },
            encoding: None,
            parents: if local == remote {
                vec![oid(local)?]
            } else {
                vec![oid(local)?, oid(remote)?]
            }
            .into_iter()
            .collect(),
            extra_headers: Vec::new(),
        };
        let id = vault
            .repo()
            .write_object(commit)
            .map_err(HistoryError::git)?
            .detach();
        gitops::sync_commit(
            vault,
            id,
            &paths.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        )?;
        Ok(id.to_string())
    }

    /// Oversized blobs in commits the peer does not yet have. Traverse
    /// each tree once without loading blob payloads; a push must never
    /// smuggle an excluded file through an earlier local commit.
    pub fn outgoing_large_paths(
        vault: &Vault,
        local: &str,
        remote: Option<&str>,
        limit: u64,
    ) -> Result<Vec<String>> {
        let known = remote
            .map(|tip| Self::ancestors(vault, tip))
            .transpose()?
            .unwrap_or_default();
        let local = Self::ancestors(vault, local)?;
        let mut trees = Vec::new();
        for id in local.keys().filter(|id| !known.contains_key(*id)) {
            trees.push((String::new(), gitops::tree_of(vault, Some(oid(id)?))?));
        }
        let mut seen = BTreeSet::new();
        let mut blobs = BTreeSet::new();
        let mut large = BTreeSet::new();
        while let Some((prefix, id)) = trees.pop() {
            if !seen.insert(id) {
                continue;
            }
            let tree = vault.repo().find_tree(id).map_err(HistoryError::git)?;
            for entry in tree.decode().map_err(HistoryError::git)?.entries {
                let name = std::str::from_utf8(entry.filename).map_err(HistoryError::git)?;
                let path = format!("{prefix}{name}");
                if entry.mode.is_tree() {
                    trees.push((format!("{path}/"), entry.oid.to_owned()));
                } else if entry.mode.is_blob()
                    && blobs.insert(entry.oid.to_owned())
                    && Self::blob_size(vault, &BlobId(entry.oid.to_string()))? > limit
                {
                    large.insert(path);
                }
            }
        }
        Ok(large.into_iter().collect())
    }

    pub fn blob_size(vault: &Vault, blob: &BlobId) -> Result<u64> {
        Ok(vault
            .repo()
            .find_header(oid(&blob.0)?)
            .map_err(HistoryError::git)?
            .size())
    }

    /// Barrier fetched loose objects and packs before publishing a ref.
    pub fn barrier_objects(vault: &Vault) -> Result<()> {
        crate::durable::barrier_tree(&vault.git_dir().join("objects"), vault.durability())
    }

    /// Retain an interrupted local plan without changing any branch.
    pub fn retain(vault: &Vault, guard: &PublisherGuard, commit: &str) -> Result<()> {
        guard.check(vault)?;
        let name = gix::refs::FullName::try_from(format!("refs/wi-sync-recovery/{commit}"))
            .map_err(HistoryError::git)?;
        let current = vault
            .repo()
            .try_find_reference(name.as_ref())
            .map_err(HistoryError::git)?;
        if current.is_none() {
            gitops::advance_branch(
                vault,
                &name,
                None,
                oid(commit)?,
                "retain interrupted synchronization",
            )?;
        }
        gitops::barrier_ref(vault, &name)
    }

    pub fn advance(
        vault: &Vault,
        guard: &PublisherGuard,
        expected: Option<&str>,
        new: &str,
    ) -> Result<()> {
        guard.check(vault)?;
        let (name, current) = gitops::branch_tip(vault)?;
        let new = oid(new)?;
        if current != Some(new) {
            gitops::advance_branch(
                vault,
                &name,
                expected.map(oid).transpose()?,
                new,
                "synchronize notes",
            )?;
        }
        gitops::barrier_ref(vault, &name)?;
        // The staging index is disposable. An external git command may
        // own index.lock; ordinary flush/recovery retries its refresh and
        // reports the warning without holding canonical publication open.
        let _ = gitops::sync_git_index(vault, new);
        Ok(())
    }
}
