use std::path::PathBuf;

use wi_history::HistoryError;

/// Result alias for this crate.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Errors from vault services. Every refusal changes nothing.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "unsupported-platform: vault services require Unix descriptor-relative containment and durability barriers"
    )]
    UnsupportedPlatform,
    #[error("{path}: protected path ({reason}); ordinary tools never write it")]
    Protected { path: String, reason: &'static str },
    #[error(
        "{path} already exists (checksum {current}); pass that checksum as the expected checksum to replace it"
    )]
    Exists { path: String, current: String },
    #[error(
        "{path} changed since it was read: expected {expected}, current {}; read it again and retry",
        current.as_deref().unwrap_or("absent")
    )]
    Stale {
        path: String,
        expected: String,
        current: Option<String>,
    },
    #[error("{path} does not exist")]
    NotFound { path: String },
    #[error("operation id {op_id} was already used for a different request; use a new id")]
    OperationMismatch { op_id: String },
    #[error(
        "{path} is locked (locked: true); the engine never edits it. Remove the key in an editor to unlock it"
    )]
    Locked { path: String },
    #[error("{path} has git conflict markers; resolve them in an editor first")]
    ConflictMarkers { path: String },
    #[error("field {field} is engine-owned and cannot be written through {ingress}")]
    EngineOwnedField {
        field: String,
        ingress: &'static str,
    },
    #[error(
        "claim content ({}) of lifecycle note {path} cannot be changed in place by an agent; create a superseding candidate",
        fields.join(", ")
    )]
    ClaimEditRequiresCandidate { path: String, fields: Vec<String> },
    #[error("{path} is terminal; create a fork with `restores` instead of editing its claim")]
    TerminalNote { path: String },
    #[error(
        "{path} is a lifecycle note; restoring it in place is refused (restore through a fork with `restores`)"
    )]
    LifecycleRevertRequiresFork { path: String },
    #[error(
        "the automatic-change cap is spent ({spent} loss event(s) in the window); earliest retry {}",
        retry_at.as_deref().unwrap_or("none")
    )]
    BudgetExhausted {
        retry_at: Option<String>,
        spent: usize,
        /// The work note holding the deferred-queue entry, when one was written.
        work: Option<String>,
    },
    #[error("lifecycle refused ({code}): {message}")]
    LifecycleRefused { code: &'static str, message: String },
    #[error("edit refused: {0}")]
    Edit(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("note is not valid UTF-8: {0}")]
    Parse(#[from] wi_note::ParseError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("index: {0}")]
    Index(#[from] wi_index::Error),
    #[error("vault config: {0}")]
    Config(#[from] wi_note::ConfigError),
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is {size} bytes, above the {limit}-byte read limit")]
    TooLarge { path: String, size: u64, limit: u64 },
    #[error("watcher: {0}")]
    Watch(String),
    #[error(
        "{path} is held by interrupted write {seq}, which crash recovery could not settle; \
         see its recovery-problem work request (seq {seq}). Writes to other paths continue, \
         and the next open retries it"
    )]
    RecoveryHeld { path: String, seq: u64 },
    #[error(
        "{0} is not an initialised vault (no .wiki-island/config.toml with vault_id); run `wi init`"
    )]
    NotInitialised(PathBuf),
}

impl StoreError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Whether a text or title query was refused for its length
    /// (`wi_index::Error::QueryTooLong`).
    pub fn is_query_too_long(&self) -> bool {
        matches!(self, Self::Index(wi_index::Error::QueryTooLong { .. }))
    }

    /// Stable machine-readable code for `--json` output and tools.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "unsupported-platform",
            Self::Protected { .. } => "protected-path",
            Self::Exists { .. } => "exists",
            Self::Stale { .. } => "stale",
            Self::NotFound { .. } => "not-found",
            Self::OperationMismatch { .. } => "operation-mismatch",
            Self::Locked { .. } => "locked",
            Self::ConflictMarkers { .. } => "conflict-markers",
            Self::EngineOwnedField { .. } => "engine-owned-field",
            Self::ClaimEditRequiresCandidate { .. } => "claim-edit-requires-candidate",
            Self::TerminalNote { .. } => "terminal-note",
            Self::LifecycleRevertRequiresFork { .. } => "lifecycle-revert-requires-fork",
            Self::BudgetExhausted { .. } => "budget-exhausted",
            Self::LifecycleRefused { code, .. } => code,
            Self::Edit(_) => "edit-refused",
            Self::Invalid(_) => "invalid-request",
            Self::Parse(_) => "parse",
            Self::History(_) => "history",
            Self::Index(wi_index::Error::QueryTooLong { .. }) => "invalid-request",
            Self::Index(_) => "index",
            Self::Config(_) => "config",
            Self::Io { .. } => "io",
            Self::TooLarge { .. } => "too-large",
            Self::Watch(_) => "watch",
            Self::RecoveryHeld { .. } => "recovery-held",
            Self::NotInitialised(_) => "not-initialised",
        }
    }
}
