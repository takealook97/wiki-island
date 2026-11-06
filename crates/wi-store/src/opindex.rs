//! Committed-operation lookup by id. The device index keeps a disposable
//! op-id → commit map; each lookup first scans only the commits added since
//! the map's last scan tip (usually none or a few), so a new operation id
//! costs one SQLite lookup instead of a walk over every commit. Without a
//! usable index the lookup walks the history, as before.

use wi_history::{History, OperationHit};
use wi_index::OperationRow;

use crate::error::Result;
use crate::vault::Vault;

impl Vault {
    /// The commit whose `Wi-Operation` trailer names `op_id`.
    pub(crate) fn committed_operation(&self, op_id: &str) -> Result<Option<OperationHit>> {
        match self.indexed_operation(op_id) {
            Ok(Some(row)) => Ok(Some(History::operation_hit(
                &self.history,
                wi_history::OperationTrailer {
                    op_id: row.op_id,
                    request_digest: row.request_digest,
                    commit: row.commit,
                    seq: row.seq,
                    path: row.path,
                    moved_from: row.moved_from,
                },
            )?)),
            Ok(None) => Ok(None),
            Err(_) => Ok(History::find_operation(&self.history, op_id)?),
        }
    }

    fn indexed_operation(&self, op_id: &str) -> Result<Option<OperationRow>> {
        let history = &self.history;
        self.with_index(|index| {
            let tip = index.operations_tip()?;
            let scan = History::operations_since(history, tip.as_deref())?;
            if scan.tip != tip {
                let rows: Vec<OperationRow> = scan
                    .operations
                    .into_iter()
                    .map(|t| OperationRow {
                        op_id: t.op_id,
                        request_digest: t.request_digest,
                        commit: t.commit,
                        seq: t.seq,
                        path: t.path,
                        moved_from: t.moved_from,
                    })
                    .collect();
                index.record_operations(!scan.reached_stop, &rows, scan.tip.as_deref())?;
            }
            Ok(index.operation(op_id)?)
        })
    }
}
