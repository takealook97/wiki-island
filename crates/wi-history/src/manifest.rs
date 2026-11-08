//! The manifest log `.git/wi/manifest.log`.
//!
//! Layout: the magic line `wi-manifest 1\n`, then frames. A frame is
//! `[payload length: u32 LE][first 8 bytes of SHA-256(payload)][payload]`;
//! the payload is one JSON record. The first record is always a `checkpoint`
//! carrying the manifest id, the next sequence id, the generation and the
//! retired prefix. Retirement rewrites the file atomically with a new
//! checkpoint followed by the byte-identical frames of unretired records.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::ops::Range;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::durable;
use crate::error::{HistoryError, Result};
use crate::lock::PublisherGuard;
use crate::vault::{BlobId, Vault, is_content_hash, validate_rel_path};

/// Internal manifest sequence id, monotonic per vault manifest.
pub type Seq = u64;

pub(crate) const MAGIC: &[u8] = b"wi-manifest 1\n";
pub(crate) const MANIFEST_FILE: &str = "manifest.log";
const FRAME_HEADER: usize = 12;
/// Smallest possible frame: a header and a one-byte payload. Bounds how many
/// records (and so sequence ids) a quarantined tail can hold.
pub(crate) const MIN_FRAME: u64 = FRAME_HEADER as u64 + 1;
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// Groups records into one bulk-import commit. The last record of the batch
/// sets `last`; a flush never commits an incomplete batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchMark {
    pub id: String,
    pub last: bool,
}

/// An engine write about to replace a working-tree file (step 6 of the
/// write path). `new_blob` must already be durable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    pub request_digest: String,
    pub actor: String,
    pub path: String,
    /// Previous path when the operation moves the note (archive); removed
    /// from the tree by the commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<String>,
    /// `sha256:` of the file before the write; `None` when it did not exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_hash: Option<String>,
    /// `sha256:` of the new bytes.
    pub new_hash: String,
    pub new_blob: BlobId,
    /// Commit whose bytes this operation restores (adds `Wi-Revert`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<BatchMark>,
    /// Visibility-loss events this operation charges against the change cap
    /// (one JSON object each). The flush writes one `Wi-Loss` trailer per event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub losses: Vec<serde_json::Value>,
}

/// An unmatched external change published by the watcher (or an import).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestRecord {
    pub actor: String,
    pub path: String,
    pub new_hash: String,
    pub new_blob: BlobId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<BatchMark>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Record {
    Checkpoint {
        manifest_id: String,
        next_seq: Seq,
        generation: Seq,
        retired_upto: Seq,
    },
    Intent {
        seq: Seq,
        time_ms: u64,
        #[serde(flatten)]
        rec: IntentRecord,
    },
    Done {
        seq: Seq,
        time_ms: u64,
    },
    Ingest {
        seq: Seq,
        time_ms: u64,
        #[serde(flatten)]
        rec: IngestRecord,
    },
    Abort {
        seq: Seq,
        time_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_hash: Option<String>,
    },
    Conflict {
        seq: Seq,
        time_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_hash: Option<String>,
    },
    ConflictAck {
        seq: Seq,
        time_ms: u64,
    },
}

impl Record {
    fn seq(&self) -> Option<Seq> {
        match self {
            Record::Checkpoint { .. } => None,
            Record::Intent { seq, .. }
            | Record::Done { seq, .. }
            | Record::Ingest { seq, .. }
            | Record::Abort { seq, .. }
            | Record::Conflict { seq, .. }
            | Record::ConflictAck { seq, .. } => Some(*seq),
        }
    }
}

/// The operation a sequence id stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpRecord {
    Intent(IntentRecord),
    Ingest(IngestRecord),
}

impl OpRecord {
    pub fn path(&self) -> &str {
        match self {
            OpRecord::Intent(r) => &r.path,
            OpRecord::Ingest(r) => &r.path,
        }
    }
    pub fn actor(&self) -> &str {
        match self {
            OpRecord::Intent(r) => &r.actor,
            OpRecord::Ingest(r) => &r.actor,
        }
    }
    pub fn new_hash(&self) -> &str {
        match self {
            OpRecord::Intent(r) => &r.new_hash,
            OpRecord::Ingest(r) => &r.new_hash,
        }
    }
    pub fn new_blob(&self) -> &BlobId {
        match self {
            OpRecord::Intent(r) => &r.new_blob,
            OpRecord::Ingest(r) => &r.new_blob,
        }
    }
    pub fn batch(&self) -> Option<&BatchMark> {
        match self {
            OpRecord::Intent(r) => r.batch.as_ref(),
            OpRecord::Ingest(r) => r.batch.as_ref(),
        }
    }
    pub fn moved_from(&self) -> Option<&str> {
        match self {
            OpRecord::Intent(r) => r.moved_from.as_deref(),
            OpRecord::Ingest(_) => None,
        }
    }
    /// Every path the operation changes: its target and a move's source.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.path()).chain(self.moved_from())
    }
    /// Whether the operation changes any of `paths`.
    pub fn touches(&self, paths: &[&str]) -> bool {
        self.paths().any(|p| paths.contains(&p))
    }
}

/// Settlement state of an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpStatus {
    /// `intent` without `done`: in flight, or crashed.
    Open,
    /// Published (`done`, or an `ingest`); committed by the next flush.
    Done,
    /// Intent dropped by recovery because the file still had `old_hash`.
    Aborted,
    /// Intent left in conflict by recovery; `acked` once a work item exists.
    Conflict {
        current_hash: Option<String>,
        acked: bool,
    },
}

/// One unretired operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub seq: Seq,
    pub time_ms: u64,
    pub op: OpRecord,
    pub status: OpStatus,
}

/// A parsed view of the manifest.
#[derive(Debug, Clone, Default)]
pub struct ManifestSnapshot {
    pub manifest_id: String,
    pub retired_upto: Seq,
    pub next_seq: Seq,
    pub generation: Seq,
    /// Unretired operations in sequence order.
    pub entries: Vec<ManifestEntry>,
    /// Bytes of the valid prefix.
    pub(crate) valid_len: u64,
    /// First invalid byte and the reason, when the tail is corrupt.
    pub(crate) corrupt: Option<(u64, String)>,
    /// Frames of non-checkpoint records: sequence id and byte range.
    pub(crate) frames: Vec<(Seq, Range<usize>)>,
}

impl ManifestSnapshot {
    pub fn entry(&self, seq: Seq) -> Option<&ManifestEntry> {
        self.entries
            .binary_search_by_key(&seq, |e| e.seq)
            .ok()
            .map(|i| &self.entries[i])
    }

    /// The first open intent, if any.
    pub fn open_intent(&self) -> Option<&ManifestEntry> {
        self.open_intents().next()
    }

    /// Every open intent in sequence order. Several can be open only when
    /// their paths are disjoint: recovery holds an intent open while one of
    /// its paths cannot be settled, and writes to other paths go on.
    pub fn open_intents(&self) -> impl Iterator<Item = &ManifestEntry> {
        self.entries.iter().filter(|e| e.status == OpStatus::Open)
    }

    /// The first open intent that changes any of `paths`.
    pub fn open_intent_touching(&self, paths: &[&str]) -> Option<&ManifestEntry> {
        self.open_intents().find(|e| e.op.touches(paths))
    }

    /// Conflicts that no work item has acknowledged yet.
    pub fn unacked_conflicts(&self) -> impl Iterator<Item = &ManifestEntry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.status, OpStatus::Conflict { acked: false, .. }))
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

pub(crate) fn encode_frame(record: &Record) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(record).map_err(|e| HistoryError::InvalidRecord {
        field: "record",
        reason: e.to_string(),
    })?;
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|&l| (l as usize) <= MAX_PAYLOAD)
        .ok_or_else(|| HistoryError::InvalidRecord {
            field: "record",
            reason: "record too large".into(),
        })?;
    let mut frame = Vec::with_capacity(FRAME_HEADER + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&Sha256::digest(&payload)[..8]);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

struct Fold {
    snap: ManifestSnapshot,
    ops: BTreeMap<Seq, ManifestEntry>,
    max_seq: Seq,
}

impl Fold {
    fn apply(&mut self, record: Record) -> std::result::Result<(), String> {
        let first = self.snap.manifest_id.is_empty();
        if first != matches!(record, Record::Checkpoint { .. }) {
            return Err(if first {
                "first record is not a checkpoint".into()
            } else {
                "checkpoint after the first record".into()
            });
        }
        let fresh_seq = |fold: &Fold, seq: Seq| -> std::result::Result<(), String> {
            if seq <= fold.max_seq || seq <= fold.snap.retired_upto {
                Err(format!("sequence id {seq} is not increasing"))
            } else {
                Ok(())
            }
        };
        match record {
            Record::Checkpoint {
                manifest_id,
                next_seq,
                generation,
                retired_upto,
            } => {
                if manifest_id.is_empty() {
                    return Err("empty manifest id".into());
                }
                self.snap.manifest_id = manifest_id;
                self.snap.next_seq = next_seq;
                self.snap.generation = generation;
                self.snap.retired_upto = retired_upto;
                self.max_seq = retired_upto;
            }
            Record::Intent { seq, time_ms, rec } => {
                fresh_seq(self, seq)?;
                let op = OpRecord::Intent(rec);
                let paths: Vec<&str> = op.paths().collect();
                if let Some(open) = self
                    .ops
                    .values()
                    .find(|e| e.status == OpStatus::Open && e.op.touches(&paths))
                {
                    return Err(format!(
                        "intent {seq} while intent {} is open on the same path",
                        open.seq
                    ));
                }
                self.max_seq = seq;
                self.ops.insert(
                    seq,
                    ManifestEntry {
                        seq,
                        time_ms,
                        op,
                        status: OpStatus::Open,
                    },
                );
            }
            Record::Ingest { seq, time_ms, rec } => {
                fresh_seq(self, seq)?;
                self.max_seq = seq;
                self.snap.generation = self.snap.generation.max(seq);
                self.ops.insert(
                    seq,
                    ManifestEntry {
                        seq,
                        time_ms,
                        op: OpRecord::Ingest(rec),
                        status: OpStatus::Done,
                    },
                );
            }
            Record::Done { seq, .. } => {
                self.settle(seq, OpStatus::Done)?;
                self.snap.generation = self.snap.generation.max(seq);
            }
            Record::Abort { seq, .. } => self.settle(seq, OpStatus::Aborted)?,
            Record::Conflict {
                seq, current_hash, ..
            } => self.settle(
                seq,
                OpStatus::Conflict {
                    current_hash,
                    acked: false,
                },
            )?,
            Record::ConflictAck { seq, .. } => match self.ops.get_mut(&seq) {
                Some(ManifestEntry {
                    status: OpStatus::Conflict { acked, .. },
                    ..
                }) if !*acked => *acked = true,
                _ => return Err(format!("conflict_ack for {seq} without an open conflict")),
            },
        }
        Ok(())
    }

    fn settle(&mut self, seq: Seq, status: OpStatus) -> std::result::Result<(), String> {
        match self.ops.get_mut(&seq) {
            Some(entry) if entry.status == OpStatus::Open => {
                entry.status = status;
                Ok(())
            }
            _ => Err(format!("settlement for {seq} without an open intent")),
        }
    }
}

/// Parse manifest bytes. Never fails: corruption is reported in the snapshot.
pub(crate) fn parse(bytes: &[u8]) -> ManifestSnapshot {
    let mut fold = Fold {
        snap: ManifestSnapshot::default(),
        ops: BTreeMap::new(),
        max_seq: 0,
    };
    let mut pos = 0usize;
    let corrupt = if !bytes.starts_with(MAGIC) {
        Some((0, "missing or unknown manifest header".to_owned()))
    } else {
        pos = MAGIC.len();
        loop {
            if pos == bytes.len() {
                break None;
            }
            match read_frame(bytes, pos) {
                Err(reason) => break Some((pos as u64, reason)),
                Ok((record, end)) => {
                    let seq = record.seq();
                    if let Err(reason) = fold.apply(record) {
                        break Some((pos as u64, reason));
                    }
                    if let Some(seq) = seq {
                        fold.snap.frames.push((seq, pos..end));
                    }
                    pos = end;
                }
            }
        }
    };
    let mut snap = fold.snap;
    if snap.manifest_id.is_empty() && corrupt.is_none() {
        return ManifestSnapshot {
            corrupt: Some((pos as u64, "manifest has no checkpoint".into())),
            ..ManifestSnapshot::default()
        };
    }
    snap.valid_len = corrupt.as_ref().map_or(bytes.len() as u64, |(o, _)| *o);
    snap.corrupt = corrupt;
    snap.next_seq = snap.next_seq.max(fold.max_seq + 1);
    snap.entries = fold.ops.into_values().collect();
    snap
}

fn read_frame(bytes: &[u8], pos: usize) -> std::result::Result<(Record, usize), String> {
    let rest = &bytes[pos..];
    if rest.len() < FRAME_HEADER {
        return Err("truncated frame header".into());
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&rest[..4]);
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_PAYLOAD {
        return Err(format!("frame length {len} exceeds the limit"));
    }
    if rest.len() < FRAME_HEADER + len {
        return Err("truncated frame payload".into());
    }
    let payload = &rest[FRAME_HEADER..FRAME_HEADER + len];
    if Sha256::digest(payload)[..8] != rest[4..FRAME_HEADER] {
        return Err("record checksum mismatch".into());
    }
    let record: Record =
        serde_json::from_slice(payload).map_err(|e| format!("undecodable record: {e}"))?;
    Ok((record, pos + FRAME_HEADER + len))
}

fn no_space(field: &'static str, value: &str) -> Result<()> {
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(HistoryError::InvalidRecord {
            field,
            reason: format!("{value:?} must be non-empty without whitespace"),
        });
    }
    Ok(())
}

fn content_hash(field: &'static str, value: &str) -> Result<()> {
    if is_content_hash(value) {
        Ok(())
    } else {
        Err(HistoryError::InvalidRecord {
            field,
            reason: format!("{value:?} is not sha256:<64 hex>"),
        })
    }
}

fn check_blob(vault: &Vault, blob: &BlobId) -> Result<()> {
    if vault.has_object(&blob.oid()?) {
        Ok(())
    } else {
        Err(HistoryError::MissingBlob(blob.0.clone()))
    }
}

fn check_batch(batch: Option<&BatchMark>) -> Result<()> {
    batch.map_or(Ok(()), |b| no_space("batch.id", &b.id))
}

impl IntentRecord {
    fn validate(&self, vault: &Vault) -> Result<()> {
        validate_rel_path(&self.path)?;
        if let Some(from) = &self.moved_from {
            validate_rel_path(from)?;
            if from == &self.path {
                return Err(HistoryError::InvalidRecord {
                    field: "moved_from",
                    reason: "equals path".into(),
                });
            }
        }
        if let Some(op_id) = &self.op_id {
            no_space("op_id", op_id)?;
        }
        no_space("request_digest", &self.request_digest)?;
        no_space("actor", &self.actor)?;
        if let Some(old) = &self.old_hash {
            content_hash("old_hash", old)?;
        }
        content_hash("new_hash", &self.new_hash)?;
        if let Some(revert) = &self.revert_of {
            no_space("revert_of", revert)?;
        }
        check_batch(self.batch.as_ref())?;
        check_blob(vault, &self.new_blob)
    }
}

impl IngestRecord {
    fn validate(&self, vault: &Vault) -> Result<()> {
        validate_rel_path(&self.path)?;
        no_space("actor", &self.actor)?;
        content_hash("new_hash", &self.new_hash)?;
        check_batch(self.batch.as_ref())?;
        check_blob(vault, &self.new_blob)
    }
}

fn new_manifest_id(vault: &Vault) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{nanos}:{}:{}", std::process::id(), vault.root().display());
    Sha256::digest(seed.as_bytes())[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Manifest operations. Appends require the publisher lock and fsync the log.
pub struct Manifest;

impl Manifest {
    pub(crate) fn path(vault: &Vault) -> PathBuf {
        vault.wi_dir().join(MANIFEST_FILE)
    }

    /// Raw bytes, or `None` when the manifest does not exist (or is empty).
    pub(crate) fn read_raw(vault: &Vault) -> Result<Option<Vec<u8>>> {
        let path = Self::path(vault);
        match std::fs::read(&path) {
            Ok(bytes) if bytes.is_empty() => Ok(None),
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(HistoryError::io(path, e)),
        }
    }

    /// Lock-free read for generation checks and pin lookups. A partially
    /// written tail is reported in the snapshot, not treated as an error.
    pub fn snapshot_unlocked(vault: &Vault) -> Result<ManifestSnapshot> {
        Ok(Self::read_raw(vault)?.map_or_else(ManifestSnapshot::default, |b| parse(&b)))
    }

    /// Create an empty manifest with a fresh id. Caller holds the publisher lock.
    pub(crate) fn create(vault: &Vault, next_seq: Seq, generation: Seq) -> Result<()> {
        vault.ensure_wi_dir()?;
        let mut bytes = MAGIC.to_vec();
        bytes.extend(encode_frame(&Record::Checkpoint {
            manifest_id: new_manifest_id(vault),
            next_seq,
            generation,
            retired_upto: next_seq.saturating_sub(1),
        })?);
        durable::write_atomic(&Self::path(vault), &bytes, vault.durability())
    }

    /// Strict snapshot under the publisher lock. Creates the manifest when
    /// missing; fails with `CorruptManifest` if the tail is corrupt.
    pub fn snapshot(vault: &Vault, guard: &PublisherGuard) -> Result<ManifestSnapshot> {
        guard.check(vault)?;
        let bytes = match Self::read_raw(vault)? {
            Some(bytes) => bytes,
            None => {
                Self::create(vault, 1, 0)?;
                Self::read_raw(vault)?.unwrap_or_default()
            }
        };
        let snap = parse(&bytes);
        if let Some((offset, reason)) = snap.corrupt {
            return Err(HistoryError::CorruptManifest { offset, reason });
        }
        Ok(snap)
    }

    fn append(vault: &Vault, snap: &ManifestSnapshot, record: &Record) -> Result<()> {
        let frame = encode_frame(record)?;
        let path = Self::path(vault);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| HistoryError::io(&path, e))?;
        let len = file
            .metadata()
            .map_err(|e| HistoryError::io(&path, e))?
            .len();
        if len != snap.valid_len {
            return Err(HistoryError::CorruptManifest {
                offset: snap.valid_len,
                reason: "manifest changed while the publisher lock was held".into(),
            });
        }
        file.write_all(&frame)
            .map_err(|e| HistoryError::io(&path, e))?;
        durable::barrier_at(&file, &path, vault.durability())
    }

    /// Refuse a record on `paths` while an open intent changes one of them.
    fn no_open_intent_on(snap: &ManifestSnapshot, paths: &[&str]) -> Result<()> {
        snap.open_intent_touching(paths)
            .map_or(Ok(()), |e| Err(HistoryError::OpenIntent { seq: e.seq }))
    }

    /// Publish a synchronized tree after its ref is durable. Sync owns the
    /// flush and publisher locks and starts only with an empty manifest.
    /// Reusing the checkpoint format keeps older vaults readable; the
    /// reserved sequence invalidates reads even though no note save ran.
    pub fn publish_sync(vault: &Vault, guard: &PublisherGuard, seq: Seq) -> Result<()> {
        let snap = Self::snapshot(vault, guard)?;
        if snap.generation >= seq {
            return Ok(());
        }
        if !snap.entries.is_empty() || snap.next_seq != seq {
            return Err(HistoryError::Git(
                "sync publication sequence changed".into(),
            ));
        }
        let mut out = MAGIC.to_vec();
        out.extend(encode_frame(&Record::Checkpoint {
            manifest_id: snap.manifest_id,
            next_seq: seq + 1,
            generation: seq,
            retired_upto: seq,
        })?);
        durable::write_atomic(&Self::path(vault), &out, vault.durability())
    }

    /// Append an `intent` record and fsync. Refused while an earlier intent
    /// on one of its paths is open (a crashed write): run
    /// `Recovery::replay` first. An open intent on other paths (one that
    /// recovery holds open) does not block it.
    pub fn append_intent(vault: &Vault, guard: &PublisherGuard, rec: IntentRecord) -> Result<Seq> {
        let snap = Self::snapshot(vault, guard)?;
        let paths: Vec<&str> = std::iter::once(rec.path.as_str())
            .chain(rec.moved_from.as_deref())
            .collect();
        Self::no_open_intent_on(&snap, &paths)?;
        rec.validate(vault)?;
        let seq = snap.next_seq;
        Self::append(
            vault,
            &snap,
            &Record::Intent {
                seq,
                time_ms: now_ms(),
                rec,
            },
        )?;
        Ok(seq)
    }

    /// Append the `done` record of an open intent and fsync: the ack point.
    pub fn append_done(vault: &Vault, guard: &PublisherGuard, seq: Seq) -> Result<()> {
        let snap = Self::snapshot(vault, guard)?;
        Self::settle(
            vault,
            &snap,
            seq,
            Record::Done {
                seq,
                time_ms: now_ms(),
            },
        )
    }

    /// Append an `abort` record for an open intent whose replace never
    /// happened (for example the exchange failed), so the caller leaves no
    /// open intent behind. The file still holds `current_hash`.
    pub fn append_abort(
        vault: &Vault,
        guard: &PublisherGuard,
        seq: Seq,
        current_hash: Option<String>,
    ) -> Result<()> {
        let snap = Self::snapshot(vault, guard)?;
        Self::settle(
            vault,
            &snap,
            seq,
            Record::Abort {
                seq,
                time_ms: now_ms(),
                current_hash,
            },
        )
    }

    pub(crate) fn settle(
        vault: &Vault,
        snap: &ManifestSnapshot,
        seq: Seq,
        record: Record,
    ) -> Result<()> {
        match snap.entry(seq) {
            Some(e) if e.status == OpStatus::Open => Self::append(vault, snap, &record),
            _ => Err(HistoryError::NotOpenIntent { seq }),
        }
    }

    /// Append a watcher or import `ingest` record (blob first) and fsync.
    /// Refused while an open intent changes the same path.
    pub fn append_ingest(vault: &Vault, guard: &PublisherGuard, rec: IngestRecord) -> Result<Seq> {
        let snap = Self::snapshot(vault, guard)?;
        Self::no_open_intent_on(&snap, &[rec.path.as_str()])?;
        rec.validate(vault)?;
        let seq = snap.next_seq;
        Self::append(
            vault,
            &snap,
            &Record::Ingest {
                seq,
                time_ms: now_ms(),
                rec,
            },
        )?;
        Ok(seq)
    }

    /// Acknowledge a recovery conflict once its work item exists. The flush
    /// stops at an unacknowledged conflict so it cannot be retired unseen.
    pub fn ack_conflict(vault: &Vault, guard: &PublisherGuard, seq: Seq) -> Result<()> {
        let snap = Self::snapshot(vault, guard)?;
        match snap.entry(seq) {
            Some(ManifestEntry {
                status: OpStatus::Conflict { acked: false, .. },
                ..
            }) => Self::append(
                vault,
                &snap,
                &Record::ConflictAck {
                    seq,
                    time_ms: now_ms(),
                },
            ),
            _ => Err(HistoryError::NotConflict { seq }),
        }
    }

    /// Retire every record with `seq <= upto` by atomically rewriting the
    /// log: new checkpoint plus the unchanged frames of later records.
    pub(crate) fn retire(vault: &Vault, guard: &PublisherGuard, upto: Seq) -> Result<()> {
        let snap = Self::snapshot(vault, guard)?;
        if upto <= snap.retired_upto {
            return Ok(());
        }
        let bytes = Self::read_raw(vault)?.unwrap_or_default();
        Self::rewrite(vault, &bytes, &snap, upto, snap.next_seq)
    }

    /// Atomically replace the log with a new checkpoint (`retired_upto`,
    /// `next_seq`) plus the byte-identical frames of `snap` after `upto`.
    /// Frames past `snap.valid_len` (a corrupt tail) are not copied.
    pub(crate) fn rewrite(
        vault: &Vault,
        bytes: &[u8],
        snap: &ManifestSnapshot,
        upto: Seq,
        next_seq: Seq,
    ) -> Result<()> {
        let mut out = MAGIC.to_vec();
        out.extend(encode_frame(&Record::Checkpoint {
            manifest_id: snap.manifest_id.clone(),
            next_seq,
            generation: snap.generation,
            retired_upto: upto,
        })?);
        for (seq, range) in &snap.frames {
            if *seq > upto {
                out.extend_from_slice(&bytes[range.clone()]);
            }
        }
        durable::write_atomic(&Self::path(vault), &out, vault.durability())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint() -> Record {
        Record::Checkpoint {
            manifest_id: "m".into(),
            next_seq: 1,
            generation: 0,
            retired_upto: 0,
        }
    }

    #[test]
    fn frames_round_trip_and_detect_damage() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(encode_frame(&checkpoint()).unwrap());
        let snap = parse(&bytes);
        assert!(snap.corrupt.is_none());
        assert_eq!(snap.manifest_id, "m");
        let last = bytes.len() - 2;
        bytes[last] ^= 0x01;
        assert!(parse(&bytes).corrupt.is_some());
    }

    fn intent(seq: Seq, path: &str, moved_from: Option<&str>) -> Record {
        Record::Intent {
            seq,
            time_ms: 0,
            rec: IntentRecord {
                op_id: None,
                request_digest: "d".into(),
                actor: "cli".into(),
                path: path.into(),
                moved_from: moved_from.map(Into::into),
                old_hash: None,
                new_hash: format!("sha256:{}", "0".repeat(64)),
                new_blob: BlobId("0".repeat(40)),
                revert_of: None,
                batch: None,
                losses: Vec::new(),
            },
        }
    }

    #[test]
    fn open_intents_must_have_disjoint_paths() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(encode_frame(&checkpoint()).unwrap());
        bytes.extend(encode_frame(&intent(1, "a.md", None)).unwrap());
        bytes.extend(encode_frame(&intent(2, "b.md", None)).unwrap());
        let snap = parse(&bytes);
        assert!(snap.corrupt.is_none(), "{:?}", snap.corrupt);
        assert_eq!(snap.open_intents().count(), 2);
        assert_eq!(snap.open_intent_touching(&["b.md"]).unwrap().seq, 2);
        bytes.extend(encode_frame(&intent(3, "c.md", Some("a.md"))).unwrap());
        let snap = parse(&bytes);
        assert!(snap.corrupt.unwrap().1.contains("same path"));
    }

    #[test]
    fn settlement_without_intent_is_corrupt() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(encode_frame(&checkpoint()).unwrap());
        bytes.extend(encode_frame(&Record::Done { seq: 4, time_ms: 0 }).unwrap());
        let snap = parse(&bytes);
        assert!(snap.corrupt.unwrap().1.contains("without an open intent"));
    }
}
