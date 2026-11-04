//! Startup replay: quarantine a corrupt tail, settle open intents, then
//! flush. Run it before accepting writes.

use std::io::Read;
use std::path::PathBuf;

use serde::Serialize;

use crate::durable;
use crate::error::{HistoryError, Result};
use crate::flush::{FlushReport, Flusher};
use crate::gitops;
use crate::lock::{FlushLock, PublisherGuard, PublisherLock};
use crate::manifest::{MIN_FRAME, Manifest, OpRecord, OpStatus, Record, Seq, now_ms, parse};
use crate::vault::{BlobId, Vault, hash_bytes, validate_rel_path};

const QUARANTINE_DIR: &str = "quarantine";

/// Supplies the current content hash of a working-tree file during replay.
/// The store implements it with its own hashing and protected-path rules.
pub trait WorkingTree {
    /// `sha256:` hash of the file at the vault-relative `path`, or `None` if absent.
    fn current_hash(&self, path: &str) -> std::io::Result<Option<String>>;
}

/// Hashes files under a root directly. A symlink never matches a record.
#[derive(Debug, Clone)]
pub struct FsWorkingTree {
    root: PathBuf,
}

impl FsWorkingTree {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl WorkingTree for FsWorkingTree {
    fn current_hash(&self, path: &str) -> std::io::Result<Option<String>> {
        let full = self.root.join(path);
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if !meta.is_file() {
            return Ok(Some(format!("non-file:{path}")));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(&full)?.read_to_end(&mut bytes)?;
        Ok(Some(hash_bytes(&bytes)))
    }
}

/// Corrupt tail bytes moved out of the manifest by this replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuarantineEntry {
    pub file: PathBuf,
    /// Offset of the first invalid byte in the manifest.
    pub offset: u64,
    pub len: u64,
    pub reason: String,
}

/// An intent whose target matched neither its old nor its new bytes. Both
/// versions are kept: the working-tree file and `new_blob`. The store opens a work
/// item, then calls `Manifest::ack_conflict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConflictEntry {
    pub seq: Seq,
    pub path: String,
    pub actor: String,
    pub old_hash: Option<String>,
    pub new_hash: String,
    pub new_blob: BlobId,
    pub current_hash: Option<String>,
}

/// Result of a replay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RecoveryReport {
    /// Tails quarantined by this replay.
    pub quarantined: Vec<QuarantineEntry>,
    /// Every file in `.git/wi/quarantine`, including earlier ones.
    pub quarantine_files: Vec<PathBuf>,
    /// Open intents completed with their recorded actor.
    pub completed: Vec<Seq>,
    /// Open intents dropped because the file still had its old bytes.
    pub dropped: Vec<Seq>,
    /// Unacknowledged conflicts, including ones from earlier replays.
    pub conflicts: Vec<ConflictEntry>,
    /// An open intent left open because its target could not be hashed
    /// (one bad path never fails the replay). A later replay retries it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unsettled: Vec<UnsettledIntent>,
    pub flush: FlushReport,
}

/// An open intent this replay could not settle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnsettledIntent {
    pub seq: Seq,
    pub path: String,
    pub error: String,
}

/// Manifest replay.
pub struct Recovery;

impl Recovery {
    /// Replay the manifest: quarantine a corrupt tail, settle the open intent
    /// with `tree`, then flush (which skips records already committed by
    /// `Wi-Seq` and retires them).
    pub fn replay(vault: &Vault, tree: &dyn WorkingTree) -> Result<RecoveryReport> {
        let lock = FlushLock::acquire(vault)?;
        let mut report = RecoveryReport::default();
        {
            let guard = PublisherLock::acquire(vault)?;
            if let Some(entry) = quarantine_tail(vault, &guard)? {
                report.quarantined.push(entry);
            }
            settle_open_intent(vault, &guard, tree, &mut report)?;
        }
        report.flush = Flusher::flush_locked(vault, &lock)?;
        let snap = {
            let guard = PublisherLock::acquire(vault)?;
            Manifest::snapshot(vault, &guard)?
        };
        report.conflicts = snap
            .unacked_conflicts()
            .filter_map(|e| match (&e.op, &e.status) {
                (OpRecord::Intent(rec), OpStatus::Conflict { current_hash, .. }) => {
                    Some(ConflictEntry {
                        seq: e.seq,
                        path: rec.path.clone(),
                        actor: rec.actor.clone(),
                        old_hash: rec.old_hash.clone(),
                        new_hash: rec.new_hash.clone(),
                        new_blob: rec.new_blob.clone(),
                        current_hash: current_hash.clone(),
                    })
                }
                _ => None,
            })
            .collect();
        report.quarantine_files = list_quarantine(vault)?;
        Ok(report)
    }
}

fn list_quarantine(vault: &Vault) -> Result<Vec<PathBuf>> {
    let dir = vault.wi_dir().join(QUARANTINE_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(HistoryError::io(dir, e)),
    };
    let mut files = Vec::new();
    for entry in entries {
        files.push(entry.map_err(|e| HistoryError::io(&dir, e))?.path());
    }
    files.sort();
    Ok(files)
}

/// Sequence high-water mark after quarantining `tail_len` bytes: the next
/// sequence id is above every id the tail could hold (each record takes at
/// least `MIN_FRAME` bytes and ids are assigned consecutively) and above
/// every id of this manifest already named by a commit. So a quarantined
/// sequence id is never handed out again, even when its record was
/// committed but the old checkpoint did not say so yet.
fn high_water_next_seq(
    vault: &Vault,
    snap: &crate::manifest::ManifestSnapshot,
    tail_len: u64,
) -> Result<Seq> {
    let mut next = snap.next_seq.saturating_add(tail_len.div_ceil(MIN_FRAME));
    if let Ok((_, tip)) = gitops::branch_tip(vault) {
        let committed = gitops::committed_seqs(vault, tip, &snap.manifest_id, snap.retired_upto)?;
        if let Some((&max, _)) = committed.last_key_value() {
            next = next.max(max.saturating_add(1));
        }
    }
    Ok(next)
}

/// Move a corrupt tail to `.git/wi/quarantine/<time_ms>-<offset>.bin` and
/// atomically rewrite the manifest as its valid prefix under a checkpoint
/// that carries the sequence high-water mark. The quarantine file is
/// durable before the manifest shrinks.
fn quarantine_tail(vault: &Vault, guard: &PublisherGuard) -> Result<Option<QuarantineEntry>> {
    guard.check(vault)?;
    let Some(bytes) = Manifest::read_raw(vault)? else {
        return Ok(None);
    };
    let snap = parse(&bytes);
    let Some((offset, reason)) = snap.corrupt.clone() else {
        return Ok(None);
    };
    let mode = vault.durability();
    let dir = vault.wi_dir().join(QUARANTINE_DIR);
    durable::create_dir_durable(&dir, mode)?;
    let tail = &bytes[usize::try_from(offset).unwrap_or(bytes.len())..];
    let file = dir.join(format!("{}-{offset}.bin", now_ms()));
    durable::write_new_durable(&file, tail, mode)?;
    if snap.manifest_id.is_empty() {
        // No valid checkpoint survived: start a new manifest with a new id,
        // so its sequence ids cannot collide with committed `Wi-Seq` values.
        Manifest::create(vault, 1, 0)?;
    } else {
        let next_seq = high_water_next_seq(vault, &snap, tail.len() as u64)?;
        Manifest::rewrite(vault, &bytes, &snap, snap.retired_upto, next_seq)?;
    }
    Ok(Some(QuarantineEntry {
        file,
        offset,
        len: tail.len() as u64,
        reason,
    }))
}

fn settle_open_intent(
    vault: &Vault,
    guard: &PublisherGuard,
    tree: &dyn WorkingTree,
    report: &mut RecoveryReport,
) -> Result<()> {
    let opens: Vec<Seq> = Manifest::snapshot(vault, guard)?
        .open_intents()
        .map(|e| e.seq)
        .collect();
    for seq in opens {
        settle_one(vault, guard, tree, report, seq)?;
    }
    Ok(())
}

/// Settle one open intent. One that cannot be hashed stays open (the flush
/// skips it) and the others are still settled.
fn settle_one(
    vault: &Vault,
    guard: &PublisherGuard,
    tree: &dyn WorkingTree,
    report: &mut RecoveryReport,
    seq: Seq,
) -> Result<()> {
    let snap = Manifest::snapshot(vault, guard)?;
    let Some(entry) = snap.entry(seq).filter(|e| e.status == OpStatus::Open) else {
        return Ok(());
    };
    let OpRecord::Intent(rec) = &entry.op else {
        return Ok(());
    };
    validate_rel_path(&rec.path)?;
    let current = match tree.current_hash(&rec.path) {
        Ok(current) => current,
        Err(e) => {
            report.unsettled.push(UnsettledIntent {
                seq,
                path: rec.path.clone(),
                error: e.to_string(),
            });
            return Ok(());
        }
    };
    let time_ms = now_ms();
    if current.as_deref() == Some(rec.new_hash.as_str()) {
        Manifest::settle(vault, &snap, seq, Record::Done { seq, time_ms })?;
        report.completed.push(seq);
    } else if current == rec.old_hash {
        Manifest::settle(
            vault,
            &snap,
            seq,
            Record::Abort {
                seq,
                time_ms,
                current_hash: current,
            },
        )?;
        report.dropped.push(seq);
    } else {
        Manifest::settle(
            vault,
            &snap,
            seq,
            Record::Conflict {
                seq,
                time_ms,
                current_hash: current,
            },
        )?;
    }
    Ok(())
}
