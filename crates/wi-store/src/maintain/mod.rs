//! Self-maintenance: one idempotent, deterministic, model-free pass that
//! keeps a vault healthy without anyone asking (`wi maintain`, and the
//! watcher on a schedule).
//!
//! In order, each stage silent when there is nothing to do: retry held
//! recovery intents; repair a pending index; check the index (rebuild only
//! when it is unusable or has no complete walk; a full pass also runs the
//! stat-walk catch-up); move work notes out of the folder older vaults
//! used; materialize the built-in maintenance procedure; the lifecycle
//! sweep (resume the queue, complete supersedes, flag conflicts, convert
//! work requests, create or refresh work items); read the notes the
//! detectors need, in chunks; procedure health and detector items, and
//! closing items whose cause is gone; procedure change detection; flush.
//! A pass that finds nothing writes nothing and commits nothing.
//!
//! In folder mode only the primary device (`primary_device` in the vault
//! config) runs the stages that write shared notes (migration, the
//! built-in procedure and every lifecycle stage); other devices run
//! recovery, the index and procedure change detection.
//!
//! With a time budget a pass stops between stages or between chunks of
//! the index rebuild, the work-note migration or the note scan, reports
//! `complete: false`
//! and records where it stopped in the per-device local directory (device-local,
//! never committed). The next budgeted pass resumes there, so a slice
//! exceeds its budget by at most one stage or chunk. A pass without a
//! budget always runs every stage.
//!
//! The scan remembers the publication mark it read at. When the vault
//! changes between slices, the next slice re-reads only the paths
//! published since that mark (or starts over when they are unknown), and
//! the health stage revalidates the scan once more before it uses it, so
//! a completed pass never judges stale notes. A handle that did not read
//! the scan itself (reopened mid-pass) reads it again in chunks.
//!
//! Every engine publication of one pass belongs to one batch, so a pass
//! that changes several notes makes one commit.

mod cursor;
mod index;
mod lifecycle;
mod migrate;
mod procedures;
mod scan;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::vault::Vault;

/// What one maintenance pass may do.
#[derive(Debug, Clone, Default)]
pub struct MaintainOptions {
    /// Also run the checks that read every file (stat-walk catch-up).
    pub full: bool,
    /// Stop between steps once this much time has passed.
    pub time_budget: Option<Duration>,
}

/// What one maintenance pass did. All zero / false when nothing changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MaintainReport {
    /// Held intents settled plus index paths repaired.
    pub repaired: usize,
    /// The device index was rebuilt from the files.
    pub index_rebuilt: bool,
    /// Work notes created (lifecycle sweep and procedure health).
    pub work_created: usize,
    /// Work notes closed because their cause is gone.
    pub work_closed: usize,
    /// A procedure note was added, changed or removed since the last pass
    /// on this device (host skills need a sync).
    pub procedures_changed: bool,
    /// Notes moved out of the folders older vaults used.
    pub migrated: usize,
    pub elapsed_ms: u64,
    /// False when the time budget stopped the pass early.
    pub complete: bool,
}

/// The stages of a pass, in order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Stage {
    #[default]
    Recovery,
    Index,
    Migrate,
    Builtin,
    Deferred,
    Supersedes,
    Conflicts,
    Requests,
    Sweep,
    Scan,
    Health,
    Procedures,
}

const STAGES: [Stage; 12] = [
    Stage::Recovery,
    Stage::Index,
    Stage::Migrate,
    Stage::Builtin,
    Stage::Deferred,
    Stage::Supersedes,
    Stage::Conflicts,
    Stage::Requests,
    Stage::Sweep,
    Stage::Scan,
    Stage::Health,
    Stage::Procedures,
];

/// Persisted progress of a budget-stopped pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    stage: Stage,
    /// Notes the scan already read (only for [`Stage::Scan`]).
    offset: usize,
    /// The pass was asked for the full checks.
    full: bool,
    /// Vault generation the scan's notes reflect (only for [`Stage::Scan`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generation: Option<u64>,
    /// Last file the migration inspected (only for [`Stage::Migrate`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<String>,
    /// Scans of this pass lost because their handle closed (reopened
    /// mid-pass). The first loss rescans in chunks; after a second one the
    /// next handle reads the rest in one slice, so a pass whose every slice
    /// runs on a new handle still finishes.
    #[serde(default, skip_serializing_if = "is_zero")]
    lost_scans: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Detector input read so far by the scan stage of this handle.
#[derive(Debug)]
pub(crate) struct Scan {
    /// Paths to read; `paths[next..]` are still unread.
    paths: Vec<String>,
    next: usize,
    /// Notes read so far, by path.
    notes: BTreeMap<String, wi_lifecycle::detect::DetectionNote>,
    /// Publications up to this mark are reflected in `notes`.
    mark: wi_history::PublicationMark,
}

impl Vault {
    /// Run one maintenance pass (see the module docs).
    pub fn maintain(&self, opts: &MaintainOptions) -> Result<MaintainReport> {
        let start = Instant::now();
        let mut report = MaintainReport::default();
        let over = || opts.time_budget.is_some_and(|b| start.elapsed() >= b);
        let generation = self.generation()?;
        let saved = if opts.time_budget.is_some() {
            self.load_cursor()
        } else {
            None
        };
        let mut cursor = saved.clone().unwrap_or_default();
        if opts.full && !cursor.full {
            cursor = Cursor {
                full: true,
                ..Cursor::default()
            };
        }
        self.maintain_basis.set(None);
        // Judged before the batch closes: its closing record is no change.
        let mut stale = None;
        let outcome = self
            .publication_batch(crate::import::MAINTAIN, || {
                let mut input = None;
                let complete =
                    self.run_stages(&mut cursor, &mut report, opts, &over, &mut input)?;
                if let Some(mark) = input.filter(|_| complete)
                    && self.input_stale(&mark)?
                {
                    stale = Some(mark.generation());
                }
                Ok(complete)
            })
            .and_then(|r| r);
        let complete = match outcome {
            Ok(complete) => complete,
            Err(e) => {
                // The next pass starts over.
                self.maintain_scan.borrow_mut().take();
                self.save_cursor(None, saved.is_some());
                return Err(e);
            }
        };
        if complete {
            self.maintain_scan.borrow_mut().take();
            self.save_cursor(None, saved.is_some());
            let basis = match stale {
                Some(generation) => generation,
                None => self.generation()?,
            };
            self.maintain_basis.set(Some(basis));
        } else if saved.as_ref() != Some(&cursor) {
            self.save_cursor(Some(&cursor), saved.is_some());
        }
        if self.generation()? != generation || self.manifest_pending() {
            self.flush()?;
        }
        report.complete = complete;
        report.elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(report)
    }

    /// Whether a note the detectors read changed after the pass read its
    /// input at `mark`. The pass's own work-note writes do not count.
    fn input_stale(&self, mark: &wi_history::PublicationMark) -> Result<bool> {
        let work = format!("{}/", self.config.work_dir().trim_matches('/'));
        Ok(match self.changed_since(mark)? {
            Some(paths) => paths
                .iter()
                .any(|p| crate::paths::is_markdown(p) && !p.starts_with(&work)),
            None => true,
        })
    }

    /// For the watcher: the generation a complete pass on this handle
    /// leaves behind, or, when a note changed under its detector input,
    /// the older generation that input reflects (so the next check runs a
    /// corrective pass). `None` before a complete pass.
    pub(crate) fn maintain_basis(&self) -> Option<u64> {
        self.maintain_basis.get()
    }

    /// Run the stages from `cursor`; false when the budget stopped them.
    /// At least one stage or chunk runs per call. `input` receives the
    /// mark the detector input reflects.
    fn run_stages(
        &self,
        cursor: &mut Cursor,
        report: &mut MaintainReport,
        opts: &MaintainOptions,
        over: &dyn Fn() -> bool,
        input: &mut Option<wi_history::PublicationMark>,
    ) -> Result<bool> {
        let mut ran = false;
        // Folder mode (S4): only the primary device writes shared notes in
        // maintenance; others keep recovery, the index and change detection.
        let primary = self.is_primary_device();
        let lifecycle = primary && self.lifecycle_on();
        let mut at = STAGES
            .iter()
            .position(|s| *s == cursor.stage)
            .unwrap_or_default();
        while let Some(&stage) = STAGES.get(at) {
            // After a second lost scan, scan and health run in one slice.
            let together = cursor.lost_scans >= 2 && matches!(stage, Stage::Scan | Stage::Health);
            if ran && !together && over() {
                cursor.stage = stage;
                return Ok(false);
            }
            ran = true;
            if stage != cursor.stage {
                cursor.stage = stage;
                cursor.offset = 0;
                cursor.generation = None;
                cursor.after = None;
            }
            match stage {
                Stage::Recovery => self.maintain_recovery(report)?,
                Stage::Index => {
                    let budget = opts.time_budget.map(|_| over);
                    if !self.maintain_index(report, opts.full || cursor.full, budget)? {
                        return Ok(false);
                    }
                }
                Stage::Migrate if primary => {
                    if !self.maintain_migrate(report, cursor, over)? {
                        return Ok(false);
                    }
                }
                Stage::Builtin if primary => self.maintain_builtin(report)?,
                Stage::Deferred if lifecycle => {
                    self.resume_deferred()?;
                }
                Stage::Supersedes if lifecycle => {
                    self.recover_supersedes()?;
                }
                Stage::Conflicts if lifecycle => {
                    self.propagate_conflicts()?;
                }
                Stage::Requests if self.lifecycle_on() => {
                    report.work_created += self.convert_work_requests()?.len();
                }
                Stage::Sweep if lifecycle => {
                    let mut summary = crate::lifecycle::SweepSummary::default();
                    self.sweep_items(&mut summary)?;
                    report.work_created += summary.work_created.len();
                }
                Stage::Scan if lifecycle => {
                    if !self.maintain_read_notes(cursor, over)? {
                        return Ok(false);
                    }
                }
                Stage::Health if lifecycle => {
                    match self.scan_input(opts.time_budget.is_some())? {
                        Some((detection, mark)) => {
                            *input = Some(mark);
                            self.maintain_lifecycle(report, detection)?;
                        }
                        None => {
                            // Too much changed since the scan, or this handle
                            // did not read it: scan (again) in chunks.
                            let scan = self.maintain_scan.borrow();
                            if scan.is_none() {
                                cursor.lost_scans += 1;
                            }
                            cursor.stage = Stage::Scan;
                            cursor.offset = scan.as_ref().map_or(0, |s| s.next);
                            cursor.generation = scan.as_ref().map(|s| s.mark.generation());
                            drop(scan);
                            at = STAGES
                                .iter()
                                .position(|s| *s == Stage::Scan)
                                .unwrap_or_default();
                            continue;
                        }
                    }
                }
                Stage::Procedures => self.maintain_procedures(report)?,
                _ => {}
            }
            at += 1;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Ingress, OpenOptions, Vault, WriteRequest};

    fn write(vault: &Vault, path: &str, text: &str, ingress: Ingress) {
        vault
            .write(WriteRequest {
                path: path.into(),
                content: text.as_bytes().to_vec(),
                expected_checksum: None,
                op_id: None,
                ingress,
            })
            .unwrap();
    }

    #[test]
    fn only_a_note_the_detectors_read_makes_the_input_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let (vault, _) = Vault::init_with(
            dir.path().join("vault"),
            OpenOptions {
                index_path: Some(dir.path().join("index.sqlite3")),
            },
        )
        .unwrap();
        let mark = vault.publication_mark().unwrap();
        assert!(!vault.input_stale(&mark).unwrap());
        let work = format!("{}/item.md", vault.device_work_dir().unwrap());
        write(
            &vault,
            &work,
            "---\ntype: work\n---\nitem\n",
            Ingress::Engine,
        );
        assert!(!vault.input_stale(&mark).unwrap(), "own work notes");
        write(&vault, "note.md", "changed after the input", Ingress::Cli);
        assert!(vault.input_stale(&mark).unwrap());
    }
}
