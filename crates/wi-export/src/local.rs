//! Device-local installation destinations; never infer them during background work.
use crate::{hosts::HostId, install::Error};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct Targets {
    pub home: PathBuf,
    pub hosts: Vec<(HostId, PathBuf)>,
    pub skill_dirs: Vec<PathBuf>,
    pub cache_root: PathBuf,
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
    pub fn load(root: &Path) -> Result<Option<Self>, Error> {
        crate::install::preflight_local(root)?;
        let path = root.join(".wiki-island/local/targets.json");
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
            Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => Err(Error::Config(
                "installation destinations must be a regular file".into(),
            )),
            Ok(_) => {
                let state: Self = serde_json::from_slice(&std::fs::read(path)?).map_err(|_| {
                    Error::Config("installation destinations could not be read".into())
                })?;
                state.validate()?;
                Ok(Some(state))
            }
        }
    }
    pub fn save(&self, root: &Path) -> Result<(), Error> {
        self.validate()?;
        let mut local = crate::install::local_dir(root)?;
        local.pop();
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        // Validate any prior destination file before atomic replacement.
        Self::load(root)?;
        crate::skills::atomic(&local.join("targets.json"), &bytes)
    }
}
