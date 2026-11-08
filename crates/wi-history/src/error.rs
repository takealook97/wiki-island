use std::path::PathBuf;

/// Result alias for this crate.
pub type Result<T, E = HistoryError> = std::result::Result<T, E>;

/// Errors from history and manifest operations.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("git error: {0}")]
    Git(String),
    #[error("invalid vault config: {reason}")]
    InvalidConfig { reason: String },
    #[error(
        "{remaining} manifest record(s) are not yet committed{}; settle them and flush before reverting",
        blocked.as_ref().map(|b| format!(" (flush blocked: {b:?})")).unwrap_or_default()
    )]
    PendingFlush {
        remaining: usize,
        blocked: Option<crate::flush::FlushBlock>,
    },
    #[cfg(test)]
    #[error("injected fault: {0}")]
    Injected(&'static str),
    #[error("{0} is not a git repository with a working tree")]
    NotARepository(PathBuf),
    #[error(
        "the history of this notes folder belongs to the copy at {other}; give this copy its own vault_id or remove the other copy"
    )]
    BoundElsewhere { other: PathBuf },
    #[error("HEAD is detached; the engine commits only to a branch")]
    DetachedHead,
    #[error("invalid vault path {path:?}: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("invalid record field {field}: {reason}")]
    InvalidRecord { field: &'static str, reason: String },
    #[error("blob {0} is not in the object database; write it with Blob::write_durable first")]
    MissingBlob(String),
    #[error("the manifest has an open intent (seq {seq}); run Recovery::replay before writing")]
    OpenIntent { seq: u64 },
    #[error("seq {seq} is not an open intent")]
    NotOpenIntent { seq: u64 },
    #[error("seq {seq} is not an unacknowledged conflict")]
    NotConflict { seq: u64 },
    #[error("the manifest is corrupt at byte {offset}: {reason}; run Recovery::replay")]
    CorruptManifest { offset: u64, reason: String },
    #[error("the publisher guard belongs to another vault")]
    ForeignGuard,
    #[error("the branch moved during the commit (expected {expected}); nothing was rewritten")]
    RefMoved { expected: String },
    #[error("{path} is not present in commit {commit}")]
    NotInCommit { path: String, commit: String },
    #[error("pin for {path} could not be resolved for {source_hash}")]
    PinUnresolved { path: String, source_hash: String },
    #[error("pin for {path} names commit {commit}, whose bytes do not match {source_hash}")]
    PinMismatch {
        path: String,
        commit: String,
        source_hash: String,
    },
}

impl HistoryError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn git(err: impl std::fmt::Display) -> Self {
        Self::Git(err.to_string())
    }
}
