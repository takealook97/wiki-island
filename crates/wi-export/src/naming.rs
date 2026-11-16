//! Per-vault app entry names: one MCP entry per vault in every host config.
use crate::local::Naming;
use std::path::{Path, PathBuf};

/// The entry name of the default vault.
pub const DEFAULT_ENTRY: &str = "wiki-island";
/// The longest vault name `wi setup --name` accepts.
pub const MAX_NAME: usize = 32;

/// Whether `name` is an entry this program writes: the default or a named one.
pub fn is_entry_name(name: &str) -> bool {
    name == DEFAULT_ENTRY
        || name
            .strip_prefix(DEFAULT_ENTRY)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// Whether `name` is a valid vault name: 1 to 32 characters from `[A-Za-z0-9_-]`.
pub fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The entry name for a vault name.
pub fn entry_for(name: &str) -> String {
    format!("{DEFAULT_ENTRY}-{name}")
}

/// An automatic name from the folder name: ASCII letters, digits, `_` and `-`,
/// lower-cased; other characters are dropped. When that is empty or equals the
/// default entry name, the first 8 characters of the vault id.
pub fn auto_name(folder: &str, vault_id: &str) -> String {
    let keep = |c: &char| c.is_ascii_alphanumeric() || *c == '_' || *c == '-';
    let name: String = folder
        .chars()
        .filter(keep)
        .map(|c| c.to_ascii_lowercase())
        .take(MAX_NAME)
        .collect();
    if name.is_empty() || name == DEFAULT_ENTRY {
        vault_id.chars().filter(keep).take(8).collect()
    } else {
        name
    }
}

/// What the resolver needs to know about one setup run.
pub struct Request<'a> {
    /// The `--name` given on the command line.
    pub requested: Option<&'a str>,
    /// The naming recorded on this device, if any.
    pub recorded: Option<&'a Naming>,
    /// The canonical vault root.
    pub root: &'a Path,
    /// The vault id, used when the folder name gives no automatic name.
    pub vault_id: &'a str,
    /// Every wiki-island entry in every detected host config, with the vault it serves.
    pub existing: &'a [(String, Option<PathBuf>)],
    /// Whether a path holds a live vault other than this one.
    pub other_vault: &'a dyn Fn(&Path) -> bool,
}

/// The naming to record and use for every host, and an entry of this vault to retire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub naming: Naming,
    pub retire: Option<String>,
}

impl Request<'_> {
    /// Whether some host's `entry` serves another live vault.
    fn taken(&self, entry: &str) -> bool {
        self.existing.iter().any(|(name, vault)| {
            name == entry && vault.as_deref().is_some_and(|v| (self.other_vault)(v))
        })
    }

    /// `wiki-island-<name>`, then `-2`, `-3`, ... until no host gives it to another vault.
    fn free_entry(&self, name: &str) -> String {
        let base = entry_for(name);
        let mut entry = base.clone();
        let mut suffix = 2;
        while self.taken(&entry) {
            entry = format!("{base}-{suffix}");
            suffix += 1;
        }
        entry
    }

    fn named(&self, name: &str) -> Naming {
        Naming {
            name: Some(name.to_owned()),
            entry: self.free_entry(name),
        }
    }

    fn unnamed(&self) -> Naming {
        if !self.taken(DEFAULT_ENTRY) {
            return Naming::default();
        }
        let folder = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.named(&auto_name(&folder, self.vault_id))
    }
}

/// Decide the entry name once per setup run.
///
/// A recorded entry is kept unless `--name` differs from the recorded name
/// (the old entry is then retired) or another live vault now holds it.
/// `--name N` gives `wiki-island-N`. Without a name, `wiki-island` is used
/// when no host gives it to another live vault; otherwise an automatic name.
/// A named entry held by another live vault gets `-2`, `-3`, ...
pub fn resolve(request: &Request<'_>) -> Resolution {
    let renamed = request.requested.is_some_and(|name| {
        request
            .recorded
            .is_none_or(|r| r.name.as_deref() != Some(name))
    });
    let naming = match (request.recorded, request.requested) {
        (Some(recorded), _) if !renamed && !request.taken(&recorded.entry) => recorded.clone(),
        (_, Some(name)) => request.named(name),
        (Some(recorded), None) => match &recorded.name {
            Some(name) => request.named(name),
            None => request.unnamed(),
        },
        (None, None) => request.unnamed(),
    };
    let retire = request
        .recorded
        .map(|recorded| recorded.entry.clone())
        .filter(|old| *old != naming.entry);
    Resolution { naming, retire }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/notes/Work Notes";
    const OTHER: &str = "/notes/personal";
    const STALE: &str = "/notes/deleted";

    fn other(path: &Path) -> bool {
        path == Path::new(OTHER)
    }

    fn entry(name: &str, vault: &str) -> (String, Option<PathBuf>) {
        (name.to_owned(), Some(PathBuf::from(vault)))
    }

    fn resolve_with(
        requested: Option<&str>,
        recorded: Option<&Naming>,
        existing: &[(String, Option<PathBuf>)],
    ) -> Resolution {
        resolve(&Request {
            requested,
            recorded,
            root: Path::new(ROOT),
            vault_id: "AbCdEf123456",
            existing,
            other_vault: &other,
        })
    }

    fn naming(name: Option<&str>, entry: &str) -> Naming {
        Naming {
            name: name.map(str::to_owned),
            entry: entry.to_owned(),
        }
    }

    #[test]
    fn recorded_entry_is_kept() {
        let recorded = naming(Some("work"), "wiki-island-work");
        let existing = [entry(DEFAULT_ENTRY, OTHER)];
        let out = resolve_with(None, Some(&recorded), &existing);
        assert_eq!(out.naming, recorded);
        assert_eq!(out.retire, None);
        let out = resolve_with(Some("work"), Some(&recorded), &existing);
        assert_eq!(out.naming, recorded);
        let default = Naming::default();
        let out = resolve_with(None, Some(&default), &[entry(DEFAULT_ENTRY, ROOT)]);
        assert_eq!(out.naming, default);
    }

    #[test]
    fn changed_name_applies_and_retires_the_old_entry() {
        let recorded = naming(Some("work"), "wiki-island-work");
        let out = resolve_with(Some("job"), Some(&recorded), &[]);
        assert_eq!(out.naming, naming(Some("job"), "wiki-island-job"));
        assert_eq!(out.retire.as_deref(), Some("wiki-island-work"));
        let out = resolve_with(Some("job"), Some(&Naming::default()), &[]);
        assert_eq!(out.retire.as_deref(), Some(DEFAULT_ENTRY));
    }

    #[test]
    fn requested_name_gives_a_named_entry() {
        let out = resolve_with(Some("work"), None, &[entry(DEFAULT_ENTRY, OTHER)]);
        assert_eq!(out.naming, naming(Some("work"), "wiki-island-work"));
        assert_eq!(out.retire, None);
    }

    #[test]
    fn default_entry_is_used_when_free_mine_or_stale() {
        for existing in [
            vec![],
            vec![entry(DEFAULT_ENTRY, ROOT)],
            vec![entry(DEFAULT_ENTRY, STALE)],
            vec![(DEFAULT_ENTRY.to_owned(), None)],
            vec![entry("wiki-island-other", OTHER)],
        ] {
            let out = resolve_with(None, None, &existing);
            assert_eq!(out.naming, Naming::default(), "{existing:?}");
        }
    }

    #[test]
    fn default_entry_of_another_live_vault_gives_an_automatic_name() {
        let existing = [entry(DEFAULT_ENTRY, OTHER)];
        let out = resolve_with(None, None, &existing);
        assert_eq!(
            out.naming,
            naming(Some("worknotes"), "wiki-island-worknotes")
        );
        for folder in ["/x/wiki-island", "/x/노트"] {
            let out = resolve(&Request {
                requested: None,
                recorded: None,
                root: Path::new(folder),
                vault_id: "AbCdEf123456",
                existing: &existing,
                other_vault: &other,
            });
            assert_eq!(out.naming, naming(Some("AbCdEf12"), "wiki-island-AbCdEf12"));
        }
    }

    #[test]
    fn recorded_entry_taken_by_another_vault_is_not_kept() {
        let recorded = Naming::default();
        let out = resolve_with(None, Some(&recorded), &[entry(DEFAULT_ENTRY, OTHER)]);
        assert_eq!(out.naming.entry, "wiki-island-worknotes");
        assert_eq!(out.retire.as_deref(), Some(DEFAULT_ENTRY));
    }

    #[test]
    fn named_entry_of_another_live_vault_gets_a_suffix() {
        let existing = [
            entry("wiki-island-work", OTHER),
            entry("wiki-island-work-2", OTHER),
            entry("wiki-island-work-3", STALE),
        ];
        let out = resolve_with(Some("work"), None, &existing);
        assert_eq!(out.naming, naming(Some("work"), "wiki-island-work-3"));
    }

    #[test]
    fn names_and_entry_names_are_checked() {
        assert!(valid_name("work_2-a"));
        assert!(valid_name(&"a".repeat(32)));
        for bad in ["", "has space", "dot.name", "노트", &"a".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(is_entry_name("wiki-island"));
        assert!(is_entry_name("wiki-island-work"));
        assert!(!is_entry_name("wiki-islander"));
        assert!(!is_entry_name("other"));
    }
}
