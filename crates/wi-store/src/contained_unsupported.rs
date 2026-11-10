//! Fail closed where descriptor-relative containment has not been implemented.
//! Vault initialization and opening reject this platform before filesystem writes.

use std::path::Path;
use wi_history::durable::Durability;

use crate::error::{Result, StoreError};

#[derive(Debug)]
pub(crate) enum Dir {}

impl Dir {
    pub(crate) fn open_parent(_root: &Path, _rel: &str) -> Result<Option<(Self, String)>> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn create_parent(
        _root: &Path,
        _rel: &str,
        _mode: Durability,
    ) -> Result<(Self, String)> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn path(&self) -> &Path {
        match *self {}
    }

    pub(crate) fn try_clone(&self) -> Result<Self> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn read_regular(&self, _name: &str) -> Result<Option<Vec<u8>>> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn read_regular_bounded(
        &self,
        _name: &str,
        _max_bytes: u64,
    ) -> Result<Option<Vec<u8>>> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn stat_regular(&self, _name: &str) -> Result<Option<(String, u64)>> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn permissions(&self, _name: &str) -> Result<Option<u32>> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn create_new(
        &self,
        _name: &str,
        _bytes: &[u8],
        _mode: Durability,
        _permissions: Option<u32>,
    ) -> Result<()> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn open_regular(&self, _name: &str) -> Result<Option<std::fs::File>> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn exists(&self, _name: &str) -> Result<bool> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn remove(&self, _name: &str) -> Result<()> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn rename(&self, _from: &str, _dest: &Dir, _to: &str) -> Result<bool> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn names(&self) -> Result<Vec<String>> {
        Err(StoreError::UnsupportedPlatform)
    }

    pub(crate) fn barrier(&self, _mode: Durability) -> Result<()> {
        Err(StoreError::UnsupportedPlatform)
    }
}

pub(crate) fn split(_rel: &str) -> Result<(Vec<&str>, &str)> {
    Err(StoreError::UnsupportedPlatform)
}

pub(crate) fn read_rel(_root: &Path, _rel: &str) -> Result<Option<Vec<u8>>> {
    Err(StoreError::UnsupportedPlatform)
}

pub(crate) fn read_rel_bounded(
    _root: &Path,
    _rel: &str,
    _max_bytes: u64,
) -> Result<Option<Vec<u8>>> {
    Err(StoreError::UnsupportedPlatform)
}
