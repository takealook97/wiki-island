//! Where a vault keeps its history and per-device state.
//!
//! `git` mode keeps history in `<vault>/.git`. `folder` mode keeps it in an
//! external git directory `<data>/vaults/<vault_id>/git` whose
//! `core.worktree` names the canonical vault root, so a file-synced folder
//! holds only notes and `.wiki-island/`. See `docs/DESIGN.md#storage-and-history`.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{HistoryError, Result};

/// Environment variable replacing the per-device data directory
/// (`<platform data dir>/wiki-island`).
pub const DATA_DIR_ENV: &str = "WI_DATA_DIR";
/// Vault config file, relative to the root.
pub(crate) const CONFIG_PATH: &str = ".wiki-island/config.toml";
/// Per-device state inside a git-mode vault (gitignored).
pub const VAULT_LOCAL_DIR: &str = ".wiki-island/local";

/// How a vault stores its history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageMode {
    /// History in `<vault>/.git`.
    Git,
    /// History in an external git directory in the per-device data dir.
    Folder,
}

impl StorageMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Folder => "folder",
        }
    }
}

/// The per-device data directory: `$WI_DATA_DIR`, else
/// `~/Library/Application Support/wiki-island` on macOS, else
/// `%LOCALAPPDATA%\wiki-island` on Windows, else
/// `$XDG_DATA_HOME/wiki-island` or `~/.local/share/wiki-island`.
pub fn data_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    platform_data_root()
}

#[cfg(windows)]
fn platform_data_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join("wiki-island"))
}

#[cfg(not(windows))]
fn platform_data_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)?;
    let base = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
    };
    Some(base.join("wiki-island"))
}

/// Resolve an explicit data root or the default one.
pub(crate) fn data_root_or_default(explicit: Option<&Path>) -> Result<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .or_else(data_root)
        .ok_or_else(|| HistoryError::InvalidConfig {
            reason: format!(
                "no per-device data directory: set {} or {DATA_DIR_ENV}",
                if cfg!(windows) {
                    "LOCALAPPDATA"
                } else {
                    "HOME"
                }
            ),
        })
}

/// `<data>/vaults/<vault_id>`: the per-device directory of one vault.
pub fn vault_state_dir(data_root: &Path, vault_id: &str) -> Result<PathBuf> {
    if vault_id.is_empty()
        || !vault_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(HistoryError::InvalidConfig {
            reason: format!("vault_id {vault_id:?} is not a safe identifier"),
        });
    }
    Ok(data_root.join("vaults").join(vault_id))
}

/// The `vault_id` from the vault config, if present.
pub fn read_vault_id(root: &Path) -> Result<Option<String>> {
    let path = root.join(CONFIG_PATH);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(HistoryError::io(path, e)),
    };
    let doc: toml_edit::DocumentMut =
        text.parse()
            .map_err(|e: toml_edit::TomlError| HistoryError::InvalidConfig {
                reason: e.to_string(),
            })?;
    Ok(doc
        .get("vault_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned))
}

/// The external git directory of a folder-mode vault, if one exists for
/// this vault id on this device.
pub fn external_git_dir(root: &Path, data_root: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(id) = read_vault_id(root)? else {
        return Ok(None);
    };
    let Ok(data) = data_root_or_default(data_root) else {
        return Ok(None);
    };
    let dir = vault_state_dir(&data, &id)?.join("git");
    if !dir.is_dir() {
        return Ok(None);
    }
    Ok(Some(
        crate::vault::canonical(&dir).map_err(|e| HistoryError::io(&dir, e))?,
    ))
}

/// The storage mode of the vault at `root` on this device, if it has history.
pub fn storage_mode(root: &Path, data_root: Option<&Path>) -> Result<Option<StorageMode>> {
    if root.join(".git").is_dir() {
        return Ok(Some(StorageMode::Git));
    }
    Ok(external_git_dir(root, data_root)?.map(|_| StorageMode::Folder))
}

/// Directory for per-device state of the vault at `root`: `.wiki-island/local`
/// in git mode (gitignored), `<data>/vaults/<vault_id>/local` in folder mode.
/// Without `data_root` this is the default data directory; an opened vault
/// knows its own (`Vault::local_dir`).
pub fn local_state_dir(root: &Path, data_root: Option<&Path>) -> Result<PathBuf> {
    if storage_mode(root, data_root)? == Some(StorageMode::Folder)
        && let Some(id) = read_vault_id(root)?
    {
        return Ok(vault_state_dir(&data_root_or_default(data_root)?, &id)?.join("local"));
    }
    Ok(root.join(VAULT_LOCAL_DIR))
}

/// This device's random id, kept in `<data>/device-id` and created on first
/// use. It names the primary device of a folder-mode vault.
pub fn device_id(data_root: Option<&Path>) -> Result<String> {
    let data = data_root_or_default(data_root)?;
    let path = data.join("device-id");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let id = text.trim();
        if !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Ok(id.to_owned());
        }
    }
    let mut bytes = [0u8; 16];
    let seed = format!(
        "{:?}{}{:?}",
        std::time::SystemTime::now(),
        std::process::id(),
        std::thread::current().id()
    );
    let digest = <sha2::Sha256 as sha2::Digest>::digest(seed.as_bytes());
    bytes.copy_from_slice(&digest[..16]);
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mode = crate::Durability::default();
    crate::durable::create_dir_durable(&data, mode)?;
    // Publish with a hard link so two first uses agree on one id.
    let tmp = data.join(format!(".device-id.{id}.tmp"));
    crate::durable::write_atomic(&tmp, format!("{id}\n").as_bytes(), mode)?;
    let linked = std::fs::hard_link(&tmp, &path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {
            crate::durable::barrier_dir(&data, mode)?;
            Ok(id)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let text = std::fs::read_to_string(&path).map_err(|e| HistoryError::io(&path, e))?;
            Ok(text.trim().to_owned())
        }
        Err(e) => Err(HistoryError::io(&path, e)),
    }
}

/// Quote a path for a git config value.
pub(crate) fn config_quote(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or_else(|| HistoryError::InvalidConfig {
        reason: format!("{} is not valid UTF-8", path.display()),
    })?;
    if text.chars().any(|c| c == '\n' || c == '\0') {
        return Err(HistoryError::InvalidConfig {
            reason: format!("{} contains a line break", path.display()),
        });
    }
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// The git config of an external git directory bound to `worktree`.
pub(crate) fn external_config(worktree: &Path) -> Result<String> {
    Ok(format!(
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n\tworktree = {}\n",
        config_quote(worktree)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_quote_escapes_quotes_and_backslashes() {
        assert_eq!(
            config_quote(Path::new("/a b/c\"d\\e")).unwrap(),
            "\"/a b/c\\\"d\\\\e\""
        );
    }

    #[test]
    fn vault_state_dir_refuses_unsafe_ids() {
        assert!(vault_state_dir(Path::new("/d"), "../x").is_err());
        assert!(vault_state_dir(Path::new("/d"), "").is_err());
        assert_eq!(
            vault_state_dir(Path::new("/d"), "ab-1").unwrap(),
            Path::new("/d/vaults/ab-1")
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn data_root_uses_local_app_data_and_honors_overrides() {
        const CHILD: &str = "WI_TEST_DATA_ROOT_CHILD";
        if let Ok(mode) = std::env::var(CHILD) {
            let expected = match mode.as_str() {
                "override" => Some(PathBuf::from(std::env::var_os(DATA_DIR_ENV).unwrap())),
                "missing" | "empty" => None,
                _ => Some(
                    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("wiki-island"),
                ),
            };
            assert_eq!(data_root(), expected);
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        for layout in ["none", "profile", "drive", "override", "missing", "empty"] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "storage::windows_tests::data_root_uses_local_app_data_and_honors_overrides",
                ])
                .env(CHILD, layout)
                .env_remove(DATA_DIR_ENV)
                .env_remove("HOME")
                .env_remove("USERPROFILE")
                .env_remove("HOMEDRIVE")
                .env_remove("HOMEPATH")
                .env("APPDATA", temp.path().join("roaming"))
                .env("LOCALAPPDATA", temp.path().join("local"));
            match layout {
                "profile" => {
                    command.env("USERPROFILE", temp.path());
                }
                "drive" => {
                    command
                        .env("HOMEDRIVE", "C:")
                        .env("HOMEPATH", "\\Users\\fixture");
                }
                "override" => {
                    command
                        .env_remove("LOCALAPPDATA")
                        .env(DATA_DIR_ENV, temp.path().join("override"));
                }
                "missing" => {
                    command.env_remove("LOCALAPPDATA");
                }
                "empty" => {
                    command.env("LOCALAPPDATA", "");
                }
                _ => {}
            }
            assert!(command.status().unwrap().success(), "{layout}");
        }
    }
}
