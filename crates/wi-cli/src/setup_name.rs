//! The vault name and app entry name used by `wi setup`, `wi uninstall` and `wi mcp install`.
use anyhow::Result;
use std::path::Path;
use wi_export::install::Destination;
use wi_export::local::{Naming, Targets};
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

/// The naming recorded on this device, best effort, for commands that only
/// read it. Naming outside the form this program writes counts as none.
pub(crate) fn recorded(root: &Path) -> Option<Naming> {
    Naming::load(root).ok().flatten().filter(naming::supported)
}

/// The recorded vault name for `wi mcp`, best effort: none when the record is missing.
pub(crate) fn recorded_name(root: &Path) -> Option<String> {
    let root = std::fs::canonicalize(root).ok()?;
    recorded(&root)?.name
}

/// The device record of the vault at `root`, read once before any app config
/// changes. `None` when this device has no record; a record that cannot be
/// read stops the command. Naming outside the form this program writes (a
/// hand-edited entry, say) is dropped, so the entry name is resolved again.
pub(crate) fn load(root: &Path) -> Result<Option<(Targets, Option<Naming>)>> {
    Ok(wi_export::local::load_record(root)?
        .map(|(targets, recorded)| (targets, Some(recorded).filter(naming::supported))))
}

/// The recorded destinations in `record`.
pub(crate) fn destinations(record: Option<&(Targets, Option<Naming>)>) -> Vec<Destination> {
    record
        .iter()
        .flat_map(|(targets, _)| &targets.hosts)
        .map(|(host, path)| Destination {
            host: *host,
            path: path.clone(),
        })
        .collect()
}

/// Decide the entry name for this vault across `destinations`.
pub(crate) fn resolve(
    root: &Path,
    requested: Option<&str>,
    recorded: Option<&Naming>,
    destinations: &[Destination],
    vault_id: &str,
) -> naming::Resolution {
    let existing: Vec<_> = destinations
        .iter()
        .flat_map(|d| install::entries(d.host, &d.path))
        .collect();
    let other_vault = |path: &Path| naming::serves_other_vault(root, path);
    naming::resolve(&naming::Request {
        requested,
        recorded,
        root,
        vault_id,
        existing: &existing,
        other_vault: &other_vault,
    })
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
}
