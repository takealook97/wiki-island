use super::MaintainReport;
use crate::error::Result;
use crate::vault::Vault;

impl Vault {
    pub(super) fn manifest_pending(&self) -> bool {
        wi_history::Manifest::snapshot_unlocked(&self.history)
            .map_or(true, |snap| !snap.entries.is_empty())
    }

    /// Retry intents the last recovery held open, then remove engine temp
    /// files a crashed writer left (dead pid, no open intent, no
    /// `recovery-problem`): they hold only unacknowledged engine bytes.
    pub(super) fn maintain_recovery(&self, report: &mut MaintainReport) -> Result<()> {
        let before = self.held.borrow().len();
        if before > 0 {
            let open = self.recover()?;
            let after = self.held.borrow().len();
            report.repaired += before.saturating_sub(after) + open.index_repaired.len();
        }
        report.repaired += self.remove_dead_temps()?.len();
        Ok(())
    }

    /// Repair a pending index; rebuild an unusable one or one without a
    /// complete walk. Never asks anyone to run a command. With a budget
    /// (`over`) the rebuild is time-sliced and resumes on later passes;
    /// false when the budget stopped it before the rebuild finished.
    pub(super) fn maintain_index(
        &self,
        report: &mut MaintainReport,
        full: bool,
        over: Option<&dyn Fn() -> bool>,
    ) -> Result<bool> {
        let Some(over) = over else {
            self.maintain_index_whole(report, full)?;
            return Ok(true);
        };
        let rebuilding = self.index_rebuilding();
        let mut restart = false;
        if !self.index_pending().is_empty() {
            match self.repair_index() {
                Ok(paths) => report.repaired += paths.len(),
                Err(_) => {
                    // An unfinished rebuild continues; otherwise begin one.
                    self.close_index();
                    restart = !rebuilding;
                }
            }
        }
        if !restart && !rebuilding && matches!(self.index_as_of(), Ok(Some(_))) {
            if full {
                self.maintain_catch_up(report)?;
            }
            return Ok(true);
        }
        match self.reindex_slice(restart, over)? {
            Some(_) => {
                report.index_rebuilt = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// [`Self::maintain_index`] without a budget: rebuild in one step.
    fn maintain_index_whole(&self, report: &mut MaintainReport, full: bool) -> Result<()> {
        if !self.index_pending().is_empty() {
            match self.repair_index() {
                Ok(paths) => report.repaired += paths.len(),
                Err(_) => {
                    self.close_index();
                    self.reindex()?;
                    report.index_rebuilt = true;
                    return Ok(());
                }
            }
        }
        let healthy = matches!(self.index_as_of(), Ok(Some(_)));
        if !healthy {
            self.close_index();
            self.reindex()?;
            report.index_rebuilt = true;
            return Ok(());
        }
        if full {
            self.maintain_catch_up(report)?;
        }
        Ok(())
    }

    /// The full pass's stat-walk catch-up.
    fn maintain_catch_up(&self, report: &mut MaintainReport) -> Result<()> {
        let sync = self.catch_up()?;
        report.repaired += sync.added + sync.updated + sync.removed;
        // Deletions no watcher saw become owner commits.
        report.repaired += self.record_missing()?.len();
        Ok(())
    }
}
