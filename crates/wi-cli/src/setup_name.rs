//! The vault name and app entry name used by `wi setup`, `wi uninstall` and `wi mcp install`.
use anyhow::Result;
use std::path::{Path, PathBuf};
use wi_export::hosts::HostId;
use wi_export::local::Naming;
use wi_export::{install, naming};

/// The sentence shown when `--name` is not a valid vault name.
pub(crate) const INVALID_NAME: &str = "The vault name must be 1 to 32 letters, digits, hyphens or underscores, with no spaces; run wi setup again with a name like --name work.";

/// Refuse an invalid `--name` before anything is written.
pub(crate) fn check(name: Option<&str>) -> Result<()> {
    match name {
        Some(name) if !naming::valid_name(name) => anyhow::bail!(INVALID_NAME),
        _ => Ok(()),
    }
}

/// The config destination of every host, as `plan_host` resolves it.
pub(crate) fn locations(
    hosts: &[HostId],
    home: &Path,
    codex_home: Option<&Path>,
) -> Result<Vec<(HostId, PathBuf)>> {
    hosts
        .iter()
        .map(|host| Ok((*host, install::config_location(*host, home, codex_home)?)))
        .collect()
}

/// The naming recorded on this device, if any. An unreadable record is
/// reported by the setup step that rewrites it.
pub(crate) fn recorded(root: &Path) -> Option<Naming> {
    Naming::load(root).ok().flatten()
}

/// The recorded vault name for `wi mcp`, best effort: none when the record is missing.
pub(crate) fn recorded_name(root: &Path) -> Option<String> {
    let root = std::fs::canonicalize(root).ok()?;
    recorded(&root)?.name
}

/// Decide the entry name for this vault across `hosts`.
pub(crate) fn resolve(
    root: &Path,
    requested: Option<&str>,
    hosts: &[(HostId, PathBuf)],
    vault_id: &str,
) -> naming::Resolution {
    let recorded = recorded(root);
    let existing: Vec<_> = hosts
        .iter()
        .flat_map(|(host, path)| install::entries(*host, path))
        .collect();
    let other_vault = |path: &Path| other_vault(root, path);
    naming::resolve(&naming::Request {
        requested,
        recorded: recorded.as_ref(),
        root,
        vault_id,
        existing: &existing,
        other_vault: &other_vault,
    })
}

/// Whether `path` holds a live vault other than the one at `root`. An entry
/// whose folder no longer holds a vault is free and is taken over.
fn other_vault(root: &Path, path: &Path) -> bool {
    let same = path == root || std::fs::canonicalize(path).is_ok_and(|p| p == root);
    !same && path.join(".wiki-island/config.toml").is_file()
}

/// Remove this vault's old entry from every recorded host. The args check in
/// `plan_remove_at` leaves an entry that serves another vault alone.
pub(crate) fn retire(root: &Path, hosts: &[(HostId, PathBuf)], entry: &str) -> Result<()> {
    for (host, path) in hosts {
        if let Some(plan) = install::plan_remove_at(*host, path, root, entry)? {
            plan.apply()?;
        }
    }
    Ok(())
}

/// The naming `wi mcp install` uses for one host: the same resolution as `wi setup`.
pub(crate) fn for_host(
    root: &Path,
    host: HostId,
    home: &Path,
    codex_home: Option<&Path>,
) -> Result<Naming> {
    let hosts = locations(&[host], home, codex_home)?;
    let id = std::fs::read_to_string(root.join(".wiki-island/config.toml"))
        .ok()
        .and_then(|text| wi_note::VaultConfig::parse(&text).ok())
        .and_then(|config| config.vault_id().map(str::to_owned))
        .unwrap_or_default();
    Ok(resolve(root, None, &hosts, &id).naming)
}

/// Record a changed naming when this device already records the vault's apps.
pub(crate) fn remember(root: &Path, naming: &Naming) -> Result<()> {
    if recorded(root).as_ref() == Some(naming) {
        return Ok(());
    }
    if let Some(targets) = wi_export::local::Targets::load(root)? {
        targets.save_named(root, naming)?;
    }
    Ok(())
}

/// Plan and check this vault's entry in every host without writing anything.
pub(crate) fn plans(
    hosts: &[HostId],
    home: &Path,
    codex_home: Option<&Path>,
    binary: &Path,
    root: &Path,
    entry: &str,
) -> Result<Vec<install::Plan>> {
    hosts
        .iter()
        .map(|host| {
            let plan = install::plan_host(*host, home, codex_home, binary, root, entry)?;
            plan.preflight()?;
            Ok(plan)
        })
        .collect()
}

/// The summary sentence naming the entry when it is not the default one.
pub(crate) fn connected(count: usize, entry: &str) -> String {
    if entry == naming::DEFAULT_ENTRY {
        format!("Connected {count} AI app(s).")
    } else {
        format!("Connected {count} AI app(s) as {entry}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_name_is_refused_with_a_plain_sentence() {
        assert!(check(None).is_ok());
        assert!(check(Some("work")).is_ok());
        let error = check(Some("my notes")).unwrap_err().to_string();
        assert_eq!(error, INVALID_NAME);
        assert!(check(Some(&"a".repeat(33))).is_err());
    }

    #[test]
    fn summary_names_only_a_non_default_entry() {
        assert_eq!(connected(2, "wiki-island"), "Connected 2 AI app(s).");
        assert_eq!(
            connected(2, "wiki-island-work"),
            "Connected 2 AI app(s) as wiki-island-work."
        );
    }

    #[test]
    fn a_folder_without_a_vault_is_not_another_vault() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mine");
        let other = temp.path().join("other");
        std::fs::create_dir_all(other.join(".wiki-island")).unwrap();
        assert!(!other_vault(&root, &other));
        std::fs::write(other.join(".wiki-island/config.toml"), "").unwrap();
        assert!(other_vault(&root, &other));
        assert!(!other_vault(&other, &other));
    }
}
