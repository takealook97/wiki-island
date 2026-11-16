//! `wi mcp install`: this vault's entry in one app, under the vault's one entry name.
//!
//! The naming and the app are recorded on this device, so `wi setup`, `wi doctor` and
//! `wi uninstall` find every entry this vault owns. When the entry name must change, every
//! recorded app gets the new name and loses this vault's old entry, as `wi setup` does.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use wi_export::hosts::HostId;
use wi_export::local::Targets;
use wi_export::{install, skills};
use wi_store::Vault;

use super::{setup_cmd, setup_name};

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
    let previous = Targets::load(vault)?;
    let mut hosts = previous
        .as_ref()
        .map(|targets| targets.hosts.clone())
        .unwrap_or_default();
    let selected = install::config_location(host, home, codex_home)?;
    if !hosts.iter().any(|(recorded, _)| *recorded == host) {
        hosts.push((host, selected));
    }
    let resolution = setup_name::resolve(vault, None, &hosts, &vault_id(vault));
    let entry = resolution.naming.entry.as_str();
    if !apply {
        let plan = install::plan_host(host, home, codex_home, binary, vault, entry)?;
        return Ok(Installed { plan, backup: None });
    }
    // A changed name applies to every recorded app that still has a config; the
    // selected app is always written.
    let migrate: Vec<HostId> = match &resolution.retire {
        Some(old) => {
            setup_name::retire(vault, &hosts, old)?;
            hosts
                .iter()
                .filter(|(other, path)| *other == host || path.is_file())
                .map(|(other, _)| *other)
                .collect()
        }
        None => vec![host],
    };
    let plans = setup_name::plans(&migrate, home, codex_home, binary, vault, entry)?;
    let mut selected = None;
    for (other, plan) in migrate.iter().zip(plans) {
        let backup = plan.apply()?;
        if *other == host {
            selected = Some(Installed { plan, backup });
        }
    }
    let installed = selected.context("The app settings could not be planned.")?;
    let (vault, _) = Vault::init(vault)?;
    let target = skills::SkillTarget::for_host(host);
    let path = installed.plan.path.clone();
    record(previous, (host, path), target, home)?.save_named(vault.root(), &resolution.naming)?;
    let targets: Vec<_> = target.into_iter().collect();
    skills::enable(&vault, &targets)?;
    skills::sync(&vault, home, &targets, true)?;
    Ok(installed)
}

/// The record with this app and its skill folder added.
fn record(
    previous: Option<Targets>,
    (host, path): (HostId, PathBuf),
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
    if !targets.hosts.iter().any(|(recorded, _)| *recorded == host) {
        targets.hosts.push((host, path));
    }
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
