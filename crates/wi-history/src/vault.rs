use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::durable::{self, Durability};
use crate::error::{HistoryError, Result};
use crate::manifest::Manifest;

/// Vault config file, relative to the root.
const CONFIG_PATH: &str = ".wiki-island/config.toml";
/// Staging directory for a new repository; `.git` appears only by rename
/// after its metadata passed the durability barrier.
const INIT_STAGING: &str = ".wiki-island-init.tmp";
/// Written in `.git/wi` after the vault root passed its barrier with `.git`.
const INIT_MARKER: &str = "init-complete";

/// The `durability` key of the vault config. A missing file or key means
/// `standard`; an unreadable file or unknown value is an error, so a vault
/// configured for `full` never silently runs weaker.
pub(crate) fn read_durability(root: &Path) -> Result<Durability> {
    let path = root.join(CONFIG_PATH);
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
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vault")
            .field("root", &self.root)
            .field("durability", &self.durability)
            .finish()
    }
}

impl Vault {
    /// Open the vault at `root`, which must hold a non-bare repository in
    /// `root/.git`. The barrier strength comes from the config key
    /// `durability` (default `standard`).
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let root = root.canonicalize().map_err(|e| HistoryError::io(root, e))?;
        let git_dir = root.join(".git");
        if !git_dir.is_dir() {
            return Err(HistoryError::NotARepository(root));
        }
        let repo = gix::open(&root).map_err(HistoryError::git)?;
        let vault = Self::from_repo(root, git_dir, repo)?;
        vault.complete_init()?;
        Ok(vault)
    }

    /// Make the `.git` entry in the vault root durable once. `init` renames
    /// a fully barriered `.git` into place, so a visible `.git` holds durable
    /// contents, but a crash or failed barrier between that rename and the
    /// root barrier leaves the entry itself unproven. The marker is written
    /// only after the root barrier, so its absence (an interrupted init, or
    /// a repository created by another tool) redoes the barrier before any
    /// write is accepted.
    fn complete_init(&self) -> Result<()> {
        let marker = self.wi_dir().join(INIT_MARKER);
        if marker.is_file() {
            return Ok(());
        }
        durable::barrier_dir(&self.root, self.durability)?;
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

    fn from_repo(root: PathBuf, git_dir: PathBuf, repo: gix::Repository) -> Result<Self> {
        if repo.workdir().is_none() {
            return Err(HistoryError::NotARepository(root));
        }
        let durability = read_durability(&root)?;
        Ok(Self {
            root,
            git_dir,
            repo,
            durability,
        })
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
