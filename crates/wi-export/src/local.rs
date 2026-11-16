//! Device-local installation destinations; never infer them during background work.
use crate::{hosts::HostId, install::Error};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct Targets {
    pub home: PathBuf,
    pub hosts: Vec<(HostId, PathBuf)>,
    pub skill_dirs: Vec<PathBuf>,
    pub cache_root: PathBuf,
}

/// The vault's name and app entry name on this device, kept in `targets.json`
/// beside [`Targets`]. Files written without these keys load as the default
/// vault: no name, entry `wiki-island`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Naming {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_entry")]
    pub entry: String,
}
impl Default for Naming {
    fn default() -> Self {
        Self {
            name: None,
            entry: default_entry(),
        }
    }
}
fn default_entry() -> String {
    crate::naming::DEFAULT_ENTRY.to_owned()
}

#[derive(Serialize)]
struct Record<'a> {
    #[serde(flatten)]
    targets: &'a Targets,
    #[serde(flatten)]
    naming: &'a Naming,
}

impl Targets {
    fn validate(&self) -> Result<(), Error> {
        if !self.home.is_absolute()
            || !self.cache_root.is_absolute()
            || self.hosts.iter().any(|(_, path)| !path.is_absolute())
            || self.skill_dirs.iter().any(|path| !path.is_absolute())
        {
            return Err(Error::Config(
                "recorded installation paths must be absolute".into(),
            ));
        }
        Ok(())
    }
    pub fn load<P: crate::install::LocalPlace + ?Sized>(root: &P) -> Result<Option<Self>, Error> {
        read_record(root)?.as_deref().map(Self::parse).transpose()
    }
    fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let state: Self = serde_json::from_slice(bytes)
            .map_err(|_| Error::Config("installation destinations could not be read".into()))?;
        state.validate()?;
        Ok(state)
    }
    /// Save the destinations and keep the recorded naming.
    pub fn save<P: crate::install::LocalPlace + ?Sized>(&self, root: &P) -> Result<(), Error> {
        let naming = Naming::load(root)?.unwrap_or_default();
        self.save_named(root, &naming)
    }
    /// Save the destinations together with the vault's naming.
    pub fn save_named<P: crate::install::LocalPlace + ?Sized>(
        &self,
        root: &P,
        naming: &Naming,
    ) -> Result<(), Error> {
        let prepared = self.prepare_named(naming)?;
        // Validate any prior destination file before atomic replacement.
        Self::load(root)?;
        prepared.write(root)
    }
    /// Check and serialize the record without touching a file, so a caller
    /// that loaded the prior record with [`load_record`] can write it later.
    pub fn prepare_named(&self, naming: &Naming) -> Result<Prepared, Error> {
        self.validate()?;
        let record = Record {
            targets: self,
            naming,
        };
        let bytes = serde_json::to_vec_pretty(&record).map_err(|e| Error::Config(e.to_string()))?;
        Ok(Prepared { bytes })
    }
}

/// A checked record, ready to replace `targets.json`.
pub struct Prepared {
    bytes: Vec<u8>,
}

impl Prepared {
    /// Replace the record atomically, creating the local folder when needed.
    pub fn write<P: crate::install::LocalPlace + ?Sized>(&self, root: &P) -> Result<(), Error> {
        let mut local = crate::install::local_dir(root)?;
        local.pop();
        crate::skills::atomic(&local.join("targets.json"), &self.bytes)
    }
}

/// The whole record, read once: destinations and naming. `None` when this
/// device has no record; a record that cannot be read is an error.
pub fn load_record<P: crate::install::LocalPlace + ?Sized>(
    root: &P,
) -> Result<Option<(Targets, Naming)>, Error> {
    let Some(bytes) = read_record(root)? else {
        return Ok(None);
    };
    Ok(Some((Targets::parse(&bytes)?, Naming::parse(&bytes)?)))
}

impl Naming {
    /// The recorded naming, or `None` when this device has no record.
    pub fn load<P: crate::install::LocalPlace + ?Sized>(root: &P) -> Result<Option<Self>, Error> {
        read_record(root)?.as_deref().map(Self::parse).transpose()
    }
    fn parse(bytes: &[u8]) -> Result<Self, Error> {
        serde_json::from_slice(bytes)
            .map_err(|_| Error::Config("installation destinations could not be read".into()))
    }
}

fn read_record<P: crate::install::LocalPlace + ?Sized>(root: &P) -> Result<Option<Vec<u8>>, Error> {
    crate::install::preflight_local(root)?;
    let path = crate::install::local_base(root).join("targets.json");
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => Err(Error::Config(
            "installation destinations must be a regular file".into(),
        )),
        Ok(_) => Ok(Some(std::fs::read(path)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(home: &std::path::Path) -> Targets {
        Targets {
            home: home.to_owned(),
            hosts: vec![(HostId::Claude, home.join(".claude.json"))],
            skill_dirs: vec![home.join(".claude/skills")],
            cache_root: home.join("cache"),
        }
    }

    fn write_record(root: &std::path::Path, text: &str) {
        let local = crate::install::local_dir(root).unwrap();
        std::fs::write(local.parent().unwrap().join("targets.json"), text).unwrap();
    }

    #[test]
    fn record_without_naming_loads_as_default_vault() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("vault");
        std::fs::create_dir_all(&root).unwrap();
        let home = temp.path().to_string_lossy().replace('\\', "\\\\");
        write_record(
            &root,
            &format!(
                r#"{{"home":"{home}","hosts":[["claude","{home}/.claude.json"]],"skill_dirs":[],"cache_root":"{home}/cache"}}"#
            ),
        );
        let state = Targets::load(&root).unwrap().unwrap();
        assert_eq!(state.hosts.len(), 1);
        assert_eq!(Naming::load(&root).unwrap(), Some(Naming::default()));
        assert_eq!(Naming::default().entry, "wiki-island");
    }

    #[test]
    fn one_app_can_record_several_config_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("vault");
        std::fs::create_dir_all(&root).unwrap();
        let mut state = targets(temp.path());
        state.hosts = vec![
            (HostId::Codex, temp.path().join(".codex/config.toml")),
            (HostId::Codex, temp.path().join("codex-new/config.toml")),
        ];
        state.save(&root).unwrap();
        let loaded = Targets::load(&root).unwrap().unwrap();
        assert_eq!(loaded.hosts, state.hosts);
    }

    #[test]
    fn naming_round_trips_and_plain_save_keeps_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("vault");
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(Naming::load(&root).unwrap(), None);
        let naming = Naming {
            name: Some("work".into()),
            entry: "wiki-island-work".into(),
        };
        targets(temp.path()).save_named(&root, &naming).unwrap();
        assert_eq!(Naming::load(&root).unwrap().as_ref(), Some(&naming));
        targets(temp.path()).save(&root).unwrap();
        assert_eq!(Naming::load(&root).unwrap(), Some(naming));
        assert!(Targets::load(&root).unwrap().is_some());
    }
}
