//! The disposable op-id map: operation id → commit, rebuilt from
//! `Wi-Operation` commit trailers by the vault service. It only speeds up a
//! lookup that a history walk can always answer, so losing it loses nothing.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Index, Result};

/// One committed operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRow {
    pub op_id: String,
    pub request_digest: String,
    pub commit: String,
    pub seq: Option<u64>,
    pub path: String,
    pub moved_from: Option<String>,
}

const TIP_KEY: &str = "operations_tip";

impl Index {
    /// The newest commit the op-id map has scanned.
    pub fn operations_tip(&self) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT value FROM index_meta WHERE key=?1",
                [TIP_KEY],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The commit that recorded `op_id`, if the map has it.
    pub fn operation(&self, op_id: &str) -> Result<Option<OperationRow>> {
        Ok(self
            .connection
            .prepare_cached(
                "SELECT op_id,request_digest,commit_id,seq,path,moved_from FROM operations WHERE op_id=?1",
            )?
            .query_row([op_id], |r| {
                Ok(OperationRow {
                    op_id: r.get(0)?,
                    request_digest: r.get(1)?,
                    commit: r.get(2)?,
                    seq: r.get::<_, Option<i64>>(3)?.and_then(|s| u64::try_from(s).ok()),
                    path: r.get(4)?,
                    moved_from: r.get(5)?,
                })
            })
            .optional()?)
    }

    /// Add scanned operations (newest first; an id keeps its newest commit)
    /// and move the scan tip, in one transaction. `reset` clears the map
    /// first (the old tip is no longer an ancestor of the branch).
    pub fn record_operations(
        &mut self,
        reset: bool,
        rows: &[OperationRow],
        tip: Option<&str>,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if reset {
            tx.execute("DELETE FROM operations", [])?;
        }
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO operations(op_id,request_digest,commit_id,seq,path,moved_from) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(op_id) DO UPDATE SET request_digest=excluded.request_digest, commit_id=excluded.commit_id, seq=excluded.seq, path=excluded.path, moved_from=excluded.moved_from",
            )?;
            for row in rows.iter().rev() {
                insert.execute(params![
                    row.op_id,
                    row.request_digest,
                    row.commit,
                    row.seq.and_then(|s| i64::try_from(s).ok()),
                    row.path,
                    row.moved_from,
                ])?;
            }
        }
        match tip {
            Some(tip) => tx.execute(
                "INSERT INTO index_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [TIP_KEY, tip],
            )?,
            None => tx.execute("DELETE FROM index_meta WHERE key=?1", [TIP_KEY])?,
        };
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(op_id: &str, commit: &str) -> OperationRow {
        OperationRow {
            op_id: op_id.into(),
            request_digest: "sha256:d".into(),
            commit: commit.into(),
            seq: Some(3),
            path: "a.md".into(),
            moved_from: None,
        }
    }

    #[test]
    fn operation_map_records_scans_and_resets() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open_in_memory(dir.path(), "v").unwrap();
        assert_eq!(index.operations_tip().unwrap(), None);
        index
            .record_operations(false, &[row("b", "c2"), row("a", "c1")], Some("c2"))
            .unwrap();
        assert_eq!(index.operations_tip().unwrap().as_deref(), Some("c2"));
        assert_eq!(index.operation("a").unwrap().unwrap().commit, "c1");
        assert!(index.operation("x").unwrap().is_none());
        index
            .record_operations(true, &[row("x", "c9")], Some("c9"))
            .unwrap();
        assert!(index.operation("a").unwrap().is_none());
        assert_eq!(index.operation("x").unwrap().unwrap().seq, Some(3));
    }
}
