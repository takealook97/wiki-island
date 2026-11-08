//! The wiki-island vault service: the single write path (temp file,
//! durable blob, publisher lock, manifest `intent`, atomic exchange, `done`,
//! index update), protected paths, engine-owned fields, file ingress with
//! watcher correlation, and current reads with generation checks.
//!
//! See `docs/OPERATIONS.md#write-path` and `crates/wi-store/README.md`.
#![forbid(unsafe_code)]

#[cfg(unix)]
mod contained;
#[cfg(not(unix))]
#[path = "contained_unsupported.rs"]
mod contained;
mod doctor;
mod error;
mod exchange;
mod fault;
mod generated;
mod import;
mod ingest;
mod ingress;
mod lease;
mod lifecycle;
mod maintain;
mod opindex;
mod opresult;
mod ops;
mod paths;
mod publish;
mod read;
mod vault;
mod watch;
mod work;

pub use doctor::{Check, DoctorReport};
pub use error::{Result, StoreError};
pub use exchange::EXCHANGE_SUPPORTED;
pub use ingest::{IngestFailure, IngestOutcome, IngestReport, ReconcileReport};
pub use lease::{LEASE_REFRESH, LEASE_TTL};
pub use lifecycle::{
    AdoptReport, CaptureInput, Charge, Classified, CurrentSearch, LintReport,
    NOTED_PREFERENCES_DIR, NOTED_RULES_DIR, NOW_ENV, NotedNote, ProcedureMode, Proposal, RuleNote,
    RuleSet, SPAN_MAX_BYTES, SnapshotInput, SweepSummary, WorkClose, WorkEntry,
};
pub use maintain::{MaintainOptions, MaintainReport};
pub use ops::{BodyEdit, EditRequest, MoveRequest, WriteRequest};
pub use paths::{EXPORT_PATHS_KEY, EXPORTS_DIR, IMPORTS_DIR, Target, is_scratch_name};
pub use read::{
    GENERATION_CHANGED, NameStat, ReadResult, SearchOptions, SearchResponse, VERIFY_LIMIT, Withheld,
};
pub use vault::{
    CACHE_DIR_ENV, Hooks, IndexState, Ingress, InitReport, OpenOptions, OpenReport,
    PRIMARY_DEVICE_KEY, Placement, Vault, WriteResult,
};
pub use watch::{
    DEBOUNCE_KEY, FLUSH_RETRY, FULL_MAINTAIN_INTERVAL, MAINTAIN_INTERVAL, MAINTAIN_SLICE,
    MaintainedHook, REPAIR_INTERVAL, REPAIR_START_DELAY, STARTUP_TIMEOUT, WatchEvent, WatchHandle,
    WatchHooks, WatchTuning,
};
pub use work::{WorkKind, WorkRequest};

pub use wi_history::{FlushReport, HistoryEntry, PublicationMark, StorageMode};
pub use wi_index::{Query, SearchHit, SearchMode, ignored, ignored_dir};
