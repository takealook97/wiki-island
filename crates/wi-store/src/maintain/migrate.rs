use super::procedures::{note_has_type, remove_empty_dirs};
use super::{Cursor, MaintainReport};
use crate::contained::read_rel;
use crate::error::Result;
use crate::ops::MoveRequest;
use crate::vault::{Ingress, Vault};

/// Files the work-note migration inspects before it checks the clock again.
const MIGRATE_CHUNK: usize = 64;

impl Vault {
    /// Move engine work notes out of the folder older vaults used (`work/`,
    /// or the old `work_dir` key) into `.wiki-island/work`. Only notes of
    /// type `work` move. Capture notes stay where they are, so path links
    /// and source pins to them keep working; new captures go to
    /// `.wiki-island/captures`.
    /// Inspects [`MIGRATE_CHUNK`] files at a time; false when the budget
    /// stopped it (`cursor.after` records the last file inspected).
    pub(super) fn maintain_migrate(
        &self,
        report: &mut MaintainReport,
        cursor: &mut Cursor,
        over: &dyn Fn() -> bool,
    ) -> Result<bool> {
        let moves = [(
            self.config.legacy_work_dir(),
            &self.device_work_dir()?,
            "work",
        )];
        for (legacy, current, kind) in moves {
            let dir = self.root().join(&legacy);
            if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let mut inspected = 0;
            for rel in crate::ingest::walk_files_in(self.root(), &dir) {
                if cursor.after.as_deref().is_some_and(|a| rel.as_str() <= a) {
                    continue;
                }
                if inspected >= MIGRATE_CHUNK && over() {
                    return Ok(false);
                }
                inspected += 1;
                cursor.after = Some(rel.clone());
                let Some(rest) = rel
                    .strip_prefix(legacy.as_str())
                    .and_then(|r| r.strip_prefix('/'))
                else {
                    continue;
                };
                if !crate::paths::is_markdown(&rel) || !note_has_type(self, &rel, kind) {
                    continue;
                }
                let to = format!("{current}/{rest}");
                if read_rel(self.root(), &to)?.is_some() {
                    continue;
                }
                self.move_note(MoveRequest {
                    from: rel,
                    to,
                    expected_checksum: None,
                    op_id: None,
                    ingress: Ingress::Engine,
                })?;
                report.migrated += 1;
            }
            remove_empty_dirs(&dir);
        }
        report.migrated += self.retire_legacy_exports()?;
        Ok(true)
    }

    /// Write the built-in maintenance procedure when it is missing or its
    /// bytes differ from the binary's text.
    pub(super) fn maintain_builtin(&self, report: &mut MaintainReport) -> Result<()> {
        let path = format!(
            "{}/{}",
            wi_note::PROCEDURES_DIR,
            wi_lifecycle::maintenance::file_name()
        );
        let want = wi_lifecycle::maintenance::note_bytes().into_bytes();
        let current = read_rel(self.root(), &path)?;
        if current.as_deref() == Some(want.as_slice()) {
            return Ok(());
        }
        let expect = current.as_deref().map(wi_history::hash_bytes);
        self.publish_engine_bytes(&path, want, expect, Ingress::Engine, None)?;
        report.procedures_changed = true;
        Ok(())
    }
}
