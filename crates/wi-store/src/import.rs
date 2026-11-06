//! Commit grouping. A bulk import, and one self-maintenance pass, each
//! publish as one commit. A separate advisory lock per kind distinguishes a
//! live batch from one whose process stopped. Closing uses an existing
//! durable blob and never rewrites a source file.
use crate::{Ingress, Result, StoreError, Vault};
use std::fs::{File, OpenOptions};
use wi_history::{BatchMark, IngestRecord, Manifest, OpStatus, PublisherLock};

/// One kind of grouped publication: which ingress it marks, its batch id
/// prefix and its lock file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BatchKind {
    ingress: Ingress,
    prefix: &'static str,
    lock: &'static str,
}

/// `wi import`: every import publication of one invocation.
pub(crate) const IMPORT: BatchKind = BatchKind {
    ingress: Ingress::Import,
    prefix: "wi-import-",
    lock: "import.lock",
};

/// One maintenance pass: every engine publication of one `maintain` call.
pub(crate) const MAINTAIN: BatchKind = BatchKind {
    ingress: Ingress::Engine,
    prefix: "wi-maintain-",
    lock: "maintain.lock",
};

const KINDS: [BatchKind; 2] = [IMPORT, MAINTAIN];

impl Vault {
    fn batch_lock(&self, kind: BatchKind) -> Result<File> {
        let path = self.history.wi_dir().join(kind.lock);
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| StoreError::io(path, e))
    }

    /// The batch mark for a publication through `ingress`, when a batch of
    /// that ingress is open on this handle.
    pub(crate) fn import_mark(&self, ingress: Ingress) -> Option<BatchMark> {
        self.import_batch_id
            .borrow()
            .as_ref()
            .filter(|(_, kind)| kind.ingress == ingress)
            .map(|(id, _)| BatchMark {
                id: id.clone(),
                last: false,
            })
    }

    /// Group import publications as one commit. This is not a filesystem
    /// transaction: a stopped invocation keeps its acknowledged partial output.
    /// Recovery closes its batch after settling any open intent.
    pub fn import_batch<T>(&self, action: impl FnOnce() -> T) -> Result<T> {
        self.publication_batch(IMPORT, action)
    }

    /// Group the publications of `kind` made by `action` as one commit.
    /// Inside an open batch the action joins it.
    pub(crate) fn publication_batch<T>(
        &self,
        kind: BatchKind,
        action: impl FnOnce() -> T,
    ) -> Result<T> {
        if self.import_batch_id.borrow().is_some() {
            return Ok(action());
        }
        let lock = self.batch_lock(kind)?;
        lock.lock()
            .map_err(|e| StoreError::io(self.history.wi_dir(), e))?;
        self.close_batches(kind)?;
        let id = format!("{}{}", kind.prefix, uuid::Uuid::new_v4());
        *self.import_batch_id.borrow_mut() = Some((id, kind));
        struct Scope<'a>(&'a std::cell::RefCell<Option<(String, BatchKind)>>);
        impl Drop for Scope<'_> {
            fn drop(&mut self) {
                *self.0.borrow_mut() = None;
            }
        }
        let scope = Scope(&self.import_batch_id);
        let result = action();
        drop(scope);
        self.close_batches(kind)?;
        Ok(result)
    }

    /// Called after replay has settled a stopped writer. Never close a live
    /// batch: its separate lock is held across all publications.
    pub(crate) fn recover_import_batches(&self) -> Result<bool> {
        let mut changed = false;
        for kind in KINDS {
            let lock = self.batch_lock(kind)?;
            match lock.try_lock() {
                Ok(()) => changed |= self.close_batches(kind)?,
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(StoreError::io(self.history.wi_dir(), e));
                }
            }
        }
        Ok(changed)
    }

    fn close_batches(&self, kind: BatchKind) -> Result<bool> {
        let actor = kind.ingress.actor();
        let guard = PublisherLock::acquire(&self.history)?;
        let snap = Manifest::snapshot(&self.history, &guard)?;
        let mut last = std::collections::BTreeMap::new();
        let mut closed = std::collections::BTreeSet::new();
        for entry in &snap.entries {
            let Some(batch) = entry.op.batch() else {
                continue;
            };
            if batch.last {
                closed.insert(batch.id.clone());
            }
            if matches!(entry.status, OpStatus::Done) {
                last.insert(batch.id.clone(), &entry.op);
            }
        }
        let mut changed = false;
        for (id, member) in last {
            if !id.starts_with(kind.prefix) || closed.contains(&id) {
                continue;
            }
            // Only this kind's own records may be completed here.
            if member.actor() != actor {
                continue;
            }
            // The closing record repeats the latest acknowledged effect,
            // including interleaved writes/moves. Repeating an older batch
            // member could resurrect a path moved away by another publisher.
            let Some(op) = snap
                .entries
                .iter()
                .rev()
                .find(|e| matches!(e.status, OpStatus::Done))
                .map(|e| &e.op)
            else {
                continue;
            };
            Manifest::append_ingest(
                &self.history,
                &guard,
                IngestRecord {
                    actor: actor.into(),
                    path: op.path().into(),
                    new_hash: op.new_hash().into(),
                    new_blob: op.new_blob().clone(),
                    batch: Some(BatchMark { id, last: true }),
                },
            )?;
            changed = true;
        }
        Ok(changed)
    }
}
