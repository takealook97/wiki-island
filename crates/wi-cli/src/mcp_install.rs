//! `wi mcp install`: this vault's entry in one app, under the vault's one entry name.
//!
//! The naming and the app's config file are recorded on this device, so `wi setup`,
//! `wi doctor` and `wi uninstall` find every entry this vault owns. Naming is applied by
//! [`naming_plan`], the routine `wi setup` uses: when the entry name must change, every
//! recorded config that holds this vault's old entry gets the new one in the same write.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use wi_export::hosts::HostId;
use wi_export::install::Destination;
use wi_export::local::Targets;
use wi_export::{install, skills};
use wi_store::Vault;

use super::{naming_plan, setup_cmd, setup_name};

/// The plan for the selected app and, when applied, its backup.
pub(crate) struct Installed {
    pub(crate) plan: install::Plan,
    pub(crate) backup: Option<PathBuf>,
}

/// Plan this vault's entry in `host`; with `apply`, write it and record it.
pub(crate) fn install(
    vault: &Path,
    host: HostId,
    home: &Path,
    codex_home: Option<&Path>,
    binary: &Path,
    apply: bool,
) -> Result<Installed> {
    let home = std::path::absolute(home)?;
    let home = home.as_path();
    // The record is read once, before anything changes; one that cannot be read stops here.
    let previous = setup_name::load(vault)?;
    let recorded = setup_name::destinations(previous.as_ref());
    let (previous, naming) = match previous {
        Some((targets, naming)) => (Some(targets), naming),
        None => (None, None),
    };
    let selected = Destination::locate(host, home, codex_home)?;
    let id = vault_id(vault);
    let planned = naming_plan::plan(&naming_plan::Request {
        root: vault,
        vault_id: &id,
        requested: None,
        naming: naming.as_ref(),
        recorded: &recorded,
        selected: std::slice::from_ref(&selected),
        binary,
    })?;
    if !apply {
        let plan = planned
            .into_plan_for(&selected)
            .context("The app settings could not be planned.")?;
        return Ok(Installed { plan, backup: None });
    }
    let (opened, _) = Vault::init(vault)?;
    let target = skills::SkillTarget::for_host(host);
    let applied = planned.apply(record(previous, target, home)?)?;
    let installed = applied
        .into_iter()
        .find(|applied| applied.destination == selected)
        .map(|applied| Installed {
            plan: applied.plan,
            backup: applied.backup,
        })
        .context("The app settings could not be planned.")?;
    let targets: Vec<_> = target.into_iter().collect();
    skills::enable(&opened, &targets)?;
    skills::sync(&opened, home, &targets, true)?;
    Ok(installed)
}

/// The record this command starts from, with the app's skill folder added.
/// The routine fills in the destinations.
fn record(
    previous: Option<Targets>,
    target: Option<skills::SkillTarget>,
    home: &Path,
) -> Result<Targets> {
    let mut targets = match previous {
        Some(targets) => targets,
        None => Targets {
            home: home.to_owned(),
            hosts: Vec::new(),
            skill_dirs: Vec::new(),
            cache_root: setup_cmd::recorded_cache_root(home)?,
        },
    };
    if let Some(dir) = target.map(|target| target.root(&targets.home))
        && !targets.skill_dirs.contains(&dir)
    {
        targets.skill_dirs.push(dir);
    }
    Ok(targets)
}

/// The vault id from its config; empty when it cannot be read yet.
fn vault_id(vault: &Path) -> String {
    std::fs::read_to_string(vault.join(".wiki-island/config.toml"))
        .ok()
        .and_then(|text| wi_note::VaultConfig::parse(&text).ok())
        .and_then(|config| config.vault_id().map(str::to_owned))
        .unwrap_or_default()
}
