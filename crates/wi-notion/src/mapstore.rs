use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use wi_history::durable::{self, Durability};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MapEntry {
    pub page_id: String,
    /// Empty until all blocks and title have been accepted remotely.
    pub content_hash: String,
    pub archived: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct State {
    version: u32,
    parent_page_id: Option<String>,
    entries: BTreeMap<String, MapEntry>,
}

/// A device-local JSON map, atomically replaced after each accepted transition.
/// The caller must serialize access to this path for the whole publish call.
pub struct MapStore {
    path: PathBuf,
    state: State,
}

impl MapStore {
    /// Missing maps start empty. Corrupt or unsupported maps are never reset.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let state = match fs::read(&path) {
            Ok(bytes) => {
                let state: State = serde_json::from_slice(&bytes)?;
                if state.version != 1 {
                    return Err(Error::Options("unsupported map version"));
                }
                for entry in state.entries.values() {
                    crate::client::validate_id(&entry.page_id)?;
                }
                state
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State {
                version: 1,
                ..State::default()
            },
            Err(error) => return Err(error.into()),
        };
        Ok(Self { path, state })
    }

    pub fn entries(&self) -> &BTreeMap<String, MapEntry> {
        &self.state.entries
    }

    pub fn get(&self, permalink: &str) -> Option<&MapEntry> {
        self.state.entries.get(permalink)
    }

    pub(crate) fn check_parent(&self, parent: &str) -> Result<()> {
        if self
            .state
            .parent_page_id
            .as_deref()
            .is_some_and(|p| p != parent)
        {
            return Err(Error::Parent);
        }
        Ok(())
    }

    pub(crate) fn bind_parent(&mut self, parent: &str) -> Result<()> {
        self.check_parent(parent)?;
        if self.state.parent_page_id.is_none() {
            self.state.parent_page_id = Some(parent.to_owned());
            if let Err(error) = self.save() {
                self.state.parent_page_id = None;
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) fn set(&mut self, permalink: &str, entry: MapEntry) -> Result<()> {
        let previous = self.state.entries.insert(permalink.to_owned(), entry);
        if let Err(error) = self.save() {
            match previous {
                Some(entry) => {
                    self.state.entries.insert(permalink.to_owned(), entry);
                }
                None => {
                    self.state.entries.remove(permalink);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        durable::create_dir_durable(parent, Durability::Standard).map_err(|_| {
            Error::Io(std::io::Error::other(
                "could not create publication state directory",
            ))
        })?;
        let path = parent.join(
            self.path
                .file_name()
                .ok_or(Error::Options("map needs a file name"))?,
        );
        durable::write_atomic(
            &path,
            &serde_json::to_vec_pretty(&self.state)?,
            Durability::Standard,
        )
        .map_err(|_| Error::Io(std::io::Error::other("could not replace publication state")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PARENT: &str = "11111111-1111-1111-1111-111111111111";
    const PAGE: &str = "22222222-2222-2222-2222-222222222222";

    #[test]
    fn rejects_corrupt_unsupported_and_cross_parent_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("map");
        fs::write(&path, b"broken").unwrap();
        assert!(MapStore::open(&path).is_err());
        fs::write(
            &path,
            br#"{"version":2,"parent_page_id":null,"entries":{}}"#,
        )
        .unwrap();
        assert!(MapStore::open(&path).is_err());
        fs::remove_file(&path).unwrap();
        let mut map = MapStore::open(&path).unwrap();
        map.bind_parent(PARENT).unwrap();
        assert!(matches!(map.check_parent(PAGE), Err(Error::Parent)));
        let original = fs::read(&path).unwrap();
        assert!(map.bind_parent(PAGE).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn failed_atomic_replacement_keeps_in_memory_state_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("map");
        let mut map = MapStore::open(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(map.bind_parent(PARENT).is_err());
        assert!(map.state.parent_page_id.is_none());
        let entry = MapEntry {
            page_id: PAGE.into(),
            content_hash: "hash".into(),
            archived: false,
        };
        assert!(map.set("note", entry.clone()).is_err());
        assert!(map.get("note").is_none());
        fs::remove_dir(&path).unwrap();
        map.bind_parent(PARENT).unwrap();
        map.set("note", entry.clone()).unwrap();
        assert_eq!(MapStore::open(&path).unwrap().get("note"), Some(&entry));
    }
}
