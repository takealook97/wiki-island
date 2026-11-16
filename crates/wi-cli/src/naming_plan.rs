//! Apply a vault's entry naming to its app configs, shared by `wi setup` and `wi mcp install`.
//!
//! Phase one ([`plan`]) resolves the entry name over every destination, reads and
//! validates every config it touches and builds every write; it changes nothing.
//! Phase two ([`NamingPlan::apply`]) writes each config once and then saves the
//! record, so `wi uninstall` finds every destination this vault has an entry in.
use anyhow::Result;
use std::path::{Path, PathBuf};
use wi_export::install::{self, Destination};
use wi_export::local::{Naming, Targets};
use wi_export::naming::Resolution;

use super::setup_name;

/// What one command asks for.
pub(crate) struct Request<'a> {
    /// The canonical vault root.
    pub(crate) root: &'a Path,
    /// The vault id; it names a vault whose folder gives no automatic name.
    pub(crate) vault_id: &'a str,
    /// The `--name` given on the command line.
    pub(crate) requested: Option<&'a str>,
    /// The naming this device records for the vault, loaded before phase one.
    pub(crate) naming: Option<&'a Naming>,
    /// The destinations this device records for the vault.
    pub(crate) recorded: &'a [Destination],
    /// The destinations this command connects; each one is always written.
    pub(crate) selected: &'a [Destination],
    /// The program the entries start.
    pub(crate) binary: &'a Path,
}

/// Every write of one command, checked and not yet applied.
pub(crate) struct NamingPlan {
    pub(crate) resolution: Resolution,
    root: PathBuf,
    writes: Vec<(Destination, install::Plan)>,
    record: Vec<Destination>,
}

/// One applied write and its backup.
pub(crate) struct Applied {
    pub(crate) destination: Destination,
    pub(crate) plan: install::Plan,
    pub(crate) backup: Option<PathBuf>,
}

/// Phase one: resolve the entry name and build every write without changing a file.
///
/// A selected destination always gets the entry. When the entry name changes,
/// a recorded destination that holds this vault's old entry gets the new one in
/// the same write. Any config that cannot be read or parsed fails the command here.
pub(crate) fn plan(request: &Request<'_>) -> Result<NamingPlan> {
    let mut all: Vec<Destination> = Vec::new();
    for destination in request.recorded.iter().chain(request.selected) {
        if !all.contains(destination) {
            all.push(destination.clone());
        }
    }
    let resolution = setup_name::resolve(
        request.root,
        request.requested,
        request.naming,
        &all,
        request.vault_id,
    );
    let entry = resolution.naming.entry.as_str();
    let retire = resolution.retire.as_deref();
    let mut writes = Vec::new();
    let mut record = Vec::new();
    for destination in &all {
        let write = request.selected.contains(destination)
            || match retire {
                Some(old) => holds(destination, request.root, old)?,
                None => false,
            };
        if write {
            let plan = install::plan_at(destination, request.binary, request.root, entry, retire)?;
            plan.preflight()?;
            writes.push((destination.clone(), plan));
            record.push(destination.clone());
        } else if serves(destination, request.root, entry) {
            record.push(destination.clone());
        }
    }
    Ok(NamingPlan {
        resolution,
        root: request.root.to_path_buf(),
        writes,
        record,
    })
}

/// Whether `destination` holds this vault's entry `entry`; a config that
/// cannot be parsed is an error.
fn holds(destination: &Destination, root: &Path, entry: &str) -> Result<bool> {
    Ok(install::plan_remove_at(destination.host, &destination.path, root, entry)?.is_some())
}

/// Whether `destination` still has this vault's `entry`, read best effort.
fn serves(destination: &Destination, root: &Path, entry: &str) -> bool {
    install::entries(destination.host, &destination.path)
        .iter()
        .any(|(name, served)| name == entry && served.as_deref() == Some(root))
}

impl NamingPlan {
    /// The planned write for `destination`, if any, without applying anything.
    pub(crate) fn into_plan_for(self, destination: &Destination) -> Option<install::Plan> {
        self.writes
            .into_iter()
            .find(|(planned, _)| planned == destination)
            .map(|(_, plan)| plan)
    }

    /// Phase two: write every planned config, then save the record with
    /// `targets`' home, skill folders and cache root.
    ///
    /// The record is checked before the first write; the prior record was
    /// read when the command started. Writes run in order. If one fails (a write error, or a config changed
    /// since phase one), the configs written before it keep the new entry and
    /// the record is not saved; running the command again finishes the job.
    pub(crate) fn apply(self, targets: Targets) -> Result<Vec<Applied>> {
        let record = Targets {
            hosts: self
                .record
                .iter()
                .map(|destination| (destination.host, destination.path.clone()))
                .collect(),
            ..targets
        }
        .prepare_named(&self.resolution.naming)?;
        let mut applied = Vec::with_capacity(self.writes.len());
        for (destination, plan) in self.writes {
            let backup = plan.apply()?;
            applied.push(Applied {
                destination,
                plan,
                backup,
            });
        }
        record.write(self.root.as_path())?;
        Ok(applied)
    }
}
