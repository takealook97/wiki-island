use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::durable::{self, Durability};
use crate::error::{HistoryError, Result};
use crate::manifest::Manifest;
use crate::storage::{self, StorageMode};

/// Staging directory for a new repository; `.git` appears only by rename
/// after its metadata passed the durability barrier.
const INIT_STAGING: &str = ".wiki-island-init.tmp";
/// Written in `.git/wi` after the vault root passed its barrier with `.git`.
const INIT_MARKER: &str = "init-complete";
/// Staging name of an external git directory, next to its final `git`.
const EXTERNAL_STAGING: &str = "git.tmp";
/// Lock serialising a re-bind of a moved folder-mode vault.
const REBIND_LOCK: &str = "rebind.lock";
/// The folder a moved folder-mode vault was bound to, for doctor.
const REBOUND_MARKER: &str = "rebound-from";

/// The `durability` key of the vault config. A missing file or key means
/// `standard`; an unreadable file or unknown value is an error, so a vault
/// configured for `full` never silently runs weaker.
pub(crate) fn read_durability(root: &Path) -> Result<Durability> {
    let path = root.join(storage::CONFIG_PATH);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Durability::default()),
        Err(e) => return Err(HistoryError::io(path, e)),
    };
    let doc: toml_edit::DocumentMut =
        text.parse()
            .map_err(|e: toml_edit::TomlError| HistoryError::InvalidConfig {
                reason: e.to_string(),
            })?;
    let Some(item) = doc.get("durability") else {
        return Ok(Durability::default());
    };
    item.as_str()
        .and_then(Durability::parse)
        .ok_or_else(|| HistoryError::InvalidConfig {
            reason: format!("durability must be \"standard\" or \"full\", found {item}"),
        })
}

/// A git object id (40 lowercase hex) of a blob.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlobId(pub String);

impl BlobId {
    pub(crate) fn oid(&self) -> Result<gix::ObjectId> {
        gix::ObjectId::from_hex(self.0.as_bytes()).map_err(|_| HistoryError::InvalidRecord {
            field: "new_blob",
            reason: format!("{:?} is not a git object id", self.0),
        })
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The git blob id `bytes` would have, without writing an object.
pub fn blob_id_of(bytes: &[u8]) -> Result<BlobId> {
    let oid = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
        .map_err(HistoryError::git)?;
    Ok(BlobId(oid.to_hex().to_string()))
}

/// `sha256:<64 lowercase hex>` of `bytes`.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(71);
    out.push_str("sha256:");
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub(crate) fn is_content_hash(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Validate a vault-relative path with `/` separators.
pub(crate) fn validate_rel_path(path: &str) -> Result<()> {
    let fail = |reason| {
        Err(HistoryError::InvalidPath {
            path: path.to_owned(),
            reason,
        })
    };
    if path.is_empty() {
        return fail("empty");
    }
    if path.starts_with('/') || path.contains('\\') {
        return fail("must be relative with '/' separators");
    }
    if path.chars().any(char::is_control) {
        return fail("contains a control character");
    }
    for part in path.split('/') {
        match part {
            "" => return fail("empty component"),
            "." | ".." => return fail("dot component"),
            ".git" => return fail("inside .git"),
            _ => {}
        }
    }
    Ok(())
}

/// An opened vault: a non-bare git repository whose working tree is the vault.
pub struct Vault {
    root: PathBuf,
    git_dir: PathBuf,
    repo: gix::Repository,
    durability: Durability,
    mode: StorageMode,
    data_root: Option<PathBuf>,
    rebound_from: Option<PathBuf>,
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vault")
            .field("root", &self.root)
            .field("mode", &self.mode)
            .field("durability", &self.durability)
            .finish()
    }
}

impl Vault {
    /// Open the vault at `root`: `root/.git` (git mode), else the external
    /// git directory registered for its `vault_id` in the per-device data
    /// directory (folder mode). The barrier strength comes from the config
    /// key `durability` (default `standard`).
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_in(root, None)
    }

    /// `open` with an explicit per-device data directory (default:
    /// [`crate::data_root`]).
    pub fn open_in(root: impl AsRef<Path>, data_root: Option<&Path>) -> Result<Self> {
        let root = root.as_ref();
        let root = root.canonicalize().map_err(|e| HistoryError::io(root, e))?;
        let git_dir = root.join(".git");
        if git_dir.is_dir() {
            let repo = gix::open(&root).map_err(HistoryError::git)?;
            let mut vault = Self::from_repo(root, git_dir, repo, StorageMode::Git)?;
            vault.data_root = data_root.map(Path::to_path_buf);
            vault.complete_init()?;
            return Ok(vault);
        }
        let Some(git_dir) = storage::external_git_dir(&root, data_root)? else {
            return Err(HistoryError::NotARepository(root));
        };
        let rebound_from = Self::bind_worktree(&root, &git_dir)?;
        let repo = gix::open(&git_dir).map_err(HistoryError::git)?;
        let mut vault = Self::from_repo(root, git_dir, repo, StorageMode::Folder)?;
        vault.data_root = data_root.map(Path::to_path_buf);
        storage::register_local_dir(&vault.root, &vault.local_dir());
        let marker = vault.wi_dir().join(REBOUND_MARKER);
        vault.rebound_from = match rebound_from {
            Some(old) => Some(old),
            None => std::fs::read_to_string(&marker)
                .ok()
                .map(|t| PathBuf::from(t.trim_end_matches('\n'))),
        };
        vault.complete_init()?;
        Ok(vault)
    }

    /// Make sure the external git directory's `core.worktree` names `root`.
    /// A vault moved on this device is re-bound under the rebind lock and
    /// remembered for doctor; a second copy with the same `vault_id` while
    /// the bound folder still exists is refused, never silently taken over.
    fn bind_worktree(root: &Path, git_dir: &Path) -> Result<Option<PathBuf>> {
        let bound = |dir: &Path| -> Result<Option<PathBuf>> {
            let repo = gix::open(dir).map_err(HistoryError::git)?;
            Ok(repo
                .workdir()
                .map(|w| w.canonicalize().unwrap_or_else(|_| w.to_path_buf())))
        };
        if bound(git_dir)?.as_deref() == Some(root) {
            return Ok(None);
        }
        let wi = git_dir.join("wi");
        durable::create_dir_durable(&wi, Durability::default())?;
        let lock_path = wi.join(REBIND_LOCK);
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| HistoryError::io(&lock_path, e))?;
        lock.lock().map_err(|e| HistoryError::io(&lock_path, e))?;
        let old = bound(git_dir)?;
        if old.as_deref() == Some(root) {
            return Ok(None);
        }
        let mode = read_durability(root)?;
        if let Some(old) = &old
            && old.join(storage::CONFIG_PATH).is_file()
            && storage::read_vault_id(old)? == storage::read_vault_id(root)?
        {
            return Err(HistoryError::BoundElsewhere { other: old.clone() });
        }
        durable::write_atomic(
            &git_dir.join("config"),
            storage::external_config(root)?.as_bytes(),
            mode,
        )?;
        let from = old.unwrap_or_default();
        durable::write_atomic(
            &wi.join(REBOUND_MARKER),
            format!("{}\n", from.display()).as_bytes(),
            mode,
        )?;
        Ok(Some(from))
    }

    /// Make the history directory entry durable once. `init` renames a
    /// fully barriered git directory into place, so a visible one holds
    /// durable contents, but a crash or failed barrier between that rename
    /// and its parent's barrier leaves the entry itself unproven. The marker
    /// is written only after the parent barrier, so its absence (an
    /// interrupted init, or a repository created by another tool) redoes the
    /// barrier before any write is accepted.
    fn complete_init(&self) -> Result<()> {
        let marker = self.wi_dir().join(INIT_MARKER);
        if marker.is_file() {
            return Ok(());
        }
        let parent = self.git_dir.parent().unwrap_or(&self.root);
        durable::barrier_dir(parent, self.durability)?;
        self.ensure_wi_dir()?;
        durable::write_atomic(&marker, b"wiki-island init complete\n", self.durability)
    }

    /// Initialise a repository at `root` if none exists, then open it.
    ///
    /// gix writes HEAD, config and the ref and object directories without
    /// fsync, so the repository is built in a staging directory, every file
    /// and directory of its `.git` passes the barrier, and only then is
    /// `.git` renamed into `root`. A crash leaves either no `.git` (the next
    /// init removes the stale staging directory and starts over) or a
    /// complete one whose root entry [`Vault::open`] makes durable.
    pub fn init(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let staging = root.join(INIT_STAGING);
        if !root.join(".git").is_dir() {
            let mode = if root.is_dir() {
                read_durability(root)?
            } else {
                Durability::default()
            };
            durable::create_dir_durable(root, mode)?;
            if staging.exists() {
                std::fs::remove_dir_all(&staging).map_err(|e| HistoryError::io(&staging, e))?;
            }
            gix::init(&staging).map_err(HistoryError::git)?;
            durable::barrier_tree(&staging.join(".git"), mode)?;
            std::fs::rename(staging.join(".git"), root.join(".git"))
                .map_err(|e| HistoryError::io(root.join(".git"), e))?;
            durable::trace::record(durable::trace::Kind::Rename, &root.join(".git"));
        }
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(|e| HistoryError::io(&staging, e))?;
        }
        Self::open(root)
    }

    /// Initialise a folder-mode vault: history in
    /// `<data>/vaults/<vault_id>/git` with `core.worktree` = `root`, so the
    /// folder holds no `.git`. The config with a `vault_id` must exist. A
    /// vault that already has `root/.git` stays in git mode.
    ///
    /// Same barrier discipline as [`Vault::init`]: the repository is built in
    /// `git.tmp` next to its final place, every file passes the barrier, then
    /// it is renamed to `git` and the parent is barriered on open.
    pub fn init_folder(root: impl AsRef<Path>, data_root: Option<&Path>) -> Result<Self> {
        let root = root.as_ref();
        let root = root.canonicalize().map_err(|e| HistoryError::io(root, e))?;
        if root.join(".git").is_dir() || storage::external_git_dir(&root, data_root)?.is_some() {
            return Self::open_in(&root, data_root);
        }
        let Some(id) = storage::read_vault_id(&root)? else {
            return Err(HistoryError::InvalidConfig {
                reason: "folder mode needs a vault_id in .wiki-island/config.toml".into(),
            });
        };
        let mode = read_durability(&root)?;
        let state = storage::vault_state_dir(&storage::data_root_or_default(data_root)?, &id)?;
        durable::create_dir_durable(&state, mode)?;
        let staging = state.join(EXTERNAL_STAGING);
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(|e| HistoryError::io(&staging, e))?;
        }
        gix::init_bare(&staging).map_err(HistoryError::git)?;
        std::fs::write(staging.join("config"), storage::external_config(&root)?)
            .map_err(|e| HistoryError::io(staging.join("config"), e))?;
        durable::barrier_tree(&staging, mode)?;
        let target = state.join("git");
        std::fs::rename(&staging, &target).map_err(|e| HistoryError::io(&target, e))?;
        durable::trace::record(durable::trace::Kind::Rename, &target);
        Self::open_in(&root, data_root)
    }

    fn from_repo(
        root: PathBuf,
        git_dir: PathBuf,
        repo: gix::Repository,
        mode: StorageMode,
    ) -> Result<Self> {
        if repo.workdir().is_none() {
            return Err(HistoryError::NotARepository(root));
        }
        let durability = read_durability(&root)?;
        Ok(Self {
            root,
            git_dir,
            repo,
            durability,
            mode,
            data_root: None,
            rebound_from: None,
        })
    }

    /// Where this vault keeps its history.
    pub fn storage_mode(&self) -> StorageMode {
        self.mode
    }

    /// The explicit per-device data directory this vault was opened with.
    pub fn data_root(&self) -> Option<&Path> {
        self.data_root.as_deref()
    }

    /// The folder this folder-mode vault was bound to before it moved on
    /// this device (empty when unknown), for doctor.
    pub fn rebound_from(&self) -> Option<&Path> {
        self.rebound_from.as_deref()
    }

    /// Per-device state: `.wiki-island/local` in git mode,
    /// `<data>/vaults/<vault_id>/local` in folder mode (next to the git dir).
    pub fn local_dir(&self) -> PathBuf {
        match self.mode {
            StorageMode::Git => self.root.join(storage::VAULT_LOCAL_DIR),
            StorageMode::Folder => self
                .git_dir
                .parent()
                .map(|p| p.join("local"))
                .unwrap_or_else(|| self.root.join(storage::VAULT_LOCAL_DIR)),
        }
    }

    /// The git exclude file honoured in addition to `.gitignore` files.
    pub fn exclude_file(&self) -> PathBuf {
        self.git_dir.join("info").join("exclude")
    }

    /// Barrier strength used for every fsync of this vault.
    pub fn durability(&self) -> Durability {
        self.durability
    }

    /// Override the configured barrier strength (for callers that pass the
    /// mode explicitly, and for measurements).
    pub fn with_durability(self, durability: Durability) -> Self {
        Self { durability, ..self }
    }

    /// The working-tree root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `.git` directory.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// `.git/wi`, holding the manifest, locks and quarantine.
    pub fn wi_dir(&self) -> PathBuf {
        self.git_dir.join("wi")
    }

    pub(crate) fn repo(&self) -> &gix::Repository {
        &self.repo
    }

    pub(crate) fn ensure_wi_dir(&self) -> Result<PathBuf> {
        let dir = self.wi_dir();
        durable::create_dir_durable(&dir, self.durability)?;
        Ok(dir)
    }

    pub(crate) fn loose_object_path(&self, oid: &gix::oid) -> PathBuf {
        let hex = oid.to_hex().to_string();
        self.git_dir.join("objects").join(&hex[..2]).join(&hex[2..])
    }

    /// Barrier a loose object, its fan-out directory and the objects
    /// directory, if it is stored loose.
    pub(crate) fn sync_object(&self, oid: &gix::oid) -> Result<()> {
        let path = self.loose_object_path(oid);
        if path.is_file() {
            durable::barrier_path(&path, self.durability)?;
            if let Some(dir) = path.parent() {
                durable::barrier_dir(dir, self.durability)?;
            }
            durable::barrier_dir(&self.git_dir.join("objects"), self.durability)?;
        }
        Ok(())
    }

    pub(crate) fn has_object(&self, oid: &gix::oid) -> bool {
        self.repo.has_object(oid)
    }

    /// Read a blob's bytes.
    pub fn read_blob(&self, blob: &BlobId) -> Result<Vec<u8>> {
        let oid = blob.oid()?;
        let found = self
            .repo
            .find_blob(oid)
            .map_err(|_| HistoryError::MissingBlob(blob.0.clone()))?;
        Ok(found.data.clone())
    }
}

/// Durable git blob writes.
pub struct Blob;

impl Blob {
    /// Store `bytes` as a git blob and make it durable (object file, fan-out
    /// directory and objects directory fsynced) before returning its id.
    pub fn write_durable(vault: &Vault, bytes: &[u8]) -> Result<BlobId> {
        let id = vault.repo.write_blob(bytes).map_err(HistoryError::git)?;
        let oid = id.detach();
        vault.sync_object(&oid)?;
        Ok(BlobId(oid.to_hex().to_string()))
    }
}

/// The vault generation: the sequence id of the last published record
/// (`done` or `ingest`). It survives retirement. Reads take no lock and ignore
/// a partially written tail.
pub fn generation(vault: &Vault) -> Result<u64> {
    Ok(Manifest::snapshot_unlocked(vault)?.generation)
}

/// The generation and the sequence id of an open intent, read together
/// without a lock. A current read is consistent only when both are equal
/// before and after it and no intent is open: an open intent may already
/// have swapped the file while the generation still names the previous
/// publication.
pub fn publication_state(vault: &Vault) -> Result<(u64, Option<u64>)> {
    let snap = Manifest::snapshot_unlocked(vault)?;
    Ok((snap.generation, snap.open_intent().map(|e| e.seq)))
}
