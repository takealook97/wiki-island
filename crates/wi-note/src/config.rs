use thiserror::Error;
use toml_edit::{DocumentMut, Item};

pub const VAULT_CONFIG_PATH: &str = ".wiki-island/config.toml";
/// Engine-owned work notes. A dot folder, so note apps hide it while it
/// still syncs with the vault.
pub const WORK_DIR: &str = ".wiki-island/work";
/// Capture notes and URL snapshots.
pub const CAPTURES_DIR: &str = ".wiki-island/captures";
/// Default archive folder (visible: archiving is a user-facing move); the
/// `archive_dir` key may name another one.
pub const ARCHIVE_DIR: &str = "archive";
/// Built-in procedures the engine materializes.
pub const PROCEDURES_DIR: &str = ".wiki-island/procedures";
/// The engine's own folder in the vault. Apart from capture notes, nothing
/// below it is delivered as a note (see [`is_hidden_engine_path`]).
pub const ENGINE_DIR: &str = ".wiki-island";
/// Folders under [`ENGINE_DIR`] the index reads as engine input (work
/// items for `wi_work`, captures that pin claim sources, built-in
/// procedures). Everything else below [`ENGINE_DIR`] (exports, import
/// records, local state) is not indexed at all.
pub const ENGINE_INPUT_DIRS: &[&str] = &[WORK_DIR, CAPTURES_DIR, PROCEDURES_DIR];

/// Whether the vault-relative `rel` lies in [`ENGINE_DIR`] (or is it).
pub fn is_engine_path(rel: &str) -> bool {
    rel.split(['/', '\\'])
        .find(|part| !part.is_empty() && *part != ".")
        .is_some_and(|first| first.eq_ignore_ascii_case(ENGINE_DIR))
}

/// Whether `rel` is an engine file that search, read, list and context
/// never deliver: anything in [`ENGINE_DIR`] except capture notes, which
/// the owner and agents save precisely to find them again.
pub fn is_hidden_engine_path(rel: &str) -> bool {
    if !is_engine_path(rel) {
        return false;
    }
    let parts: Vec<&str> = rel
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let captures: Vec<&str> = CAPTURES_DIR.split('/').collect();
    !(parts.len() > captures.len()
        && parts
            .iter()
            .zip(&captures)
            .all(|(a, b)| a.eq_ignore_ascii_case(b)))
}

/// Whether the index skips `rel`: an engine path that is neither in one of
/// the [`ENGINE_INPUT_DIRS`] nor a folder on the way to one.
pub fn is_unindexed_engine_path(rel: &str) -> bool {
    if !is_engine_path(rel) {
        return false;
    }
    let parts: Vec<&str> = rel
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    !ENGINE_INPUT_DIRS.iter().any(|dir| {
        let dir: Vec<&str> = dir.split('/').collect();
        let n = parts.len().min(dir.len());
        parts[..n]
            .iter()
            .zip(&dir[..n])
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Where work notes lived before they moved under `.wiki-island/`.
pub const LEGACY_WORK_DIR: &str = "work";
/// Where capture notes lived before they moved under `.wiki-island/`.
pub const LEGACY_CAPTURES_DIR: &str = "captures";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid TOML: {0}")]
    Parse(#[from] toml_edit::TomlError),
    #[error("invalid vault config field: {0}")]
    Field(String),
}

/// Editable TOML document; unknown keys, tables and comments are retained.
#[derive(Debug, Clone)]
pub struct VaultConfig {
    document: DocumentMut,
}

impl VaultConfig {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config = Self {
            document: text.parse()?,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn vault_id(&self) -> Option<&str> {
        self.document.get("vault_id").and_then(Item::as_str)
    }
    pub fn lifecycle(&self) -> &str {
        self.string("lifecycle", "on")
    }
    pub fn stale_after_days(&self) -> i64 {
        self.document
            .get("stale_after_days")
            .and_then(Item::as_integer)
            .unwrap_or(30)
    }
    /// Unmeasured policy default; the index skips larger notes with diagnostics.
    pub fn max_note_bytes(&self) -> u64 {
        self.document
            .get("max_note_bytes")
            .and_then(Item::as_integer)
            .unwrap_or(8 * 1024 * 1024) as u64
    }
    /// Capture directory (fixed; see [`CAPTURES_DIR`]).
    pub fn captures_dir(&self) -> &str {
        CAPTURES_DIR
    }
    /// Work note directory (fixed; see [`WORK_DIR`]).
    pub fn work_dir(&self) -> &str {
        WORK_DIR
    }
    /// Archive directory: the `archive_dir` key when it names a safe
    /// relative directory, else [`ARCHIVE_DIR`]. Notes under it are
    /// archived whatever their status.
    pub fn archive_dir(&self) -> &str {
        match self.document.get("archive_dir").and_then(Item::as_str) {
            Some(path) if safe_dir(path) => path.trim_matches('/'),
            _ => ARCHIVE_DIR,
        }
    }
    /// The work directory of a vault written before work notes moved under
    /// `.wiki-island/`: the old `work_dir` key when it names a safe
    /// relative directory, else `work`. Read only to migrate it.
    pub fn legacy_work_dir(&self) -> String {
        self.legacy_dir("work_dir", LEGACY_WORK_DIR)
    }
    /// The capture directory of a vault written before captures moved
    /// under `.wiki-island/` (old `captures_dir` key, else `captures`).
    pub fn legacy_captures_dir(&self) -> String {
        self.legacy_dir("captures_dir", LEGACY_CAPTURES_DIR)
    }
    fn legacy_dir(&self, key: &str, default: &str) -> String {
        let Some(path) = self.document.get(key).and_then(Item::as_str) else {
            return default.into();
        };
        let normalized = path
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect::<Vec<_>>()
            .join("/");
        if safe_dir(&normalized) && !path.starts_with('/') && !path.contains(['\\', '\0', ':']) {
            normalized
        } else {
            default.into()
        }
    }
    pub fn get(&self, key: &str) -> Option<&Item> {
        self.document.get(key)
    }
    pub fn to_text(&self) -> String {
        self.document.to_string()
    }

    /// Return a new config; reject invalid known fields without changing the original.
    pub fn set(&self, key: &str, value: toml_edit::Value) -> Result<Self, ConfigError> {
        let mut next = self.clone();
        let mut value = value;
        if let Some(old) = next.document.get(key).and_then(Item::as_value) {
            *value.decor_mut() = old.decor().clone();
        }
        next.document[key] = Item::Value(value);
        next.validate()?;
        Ok(next)
    }

    fn string<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.document
            .get(key)
            .and_then(Item::as_str)
            .unwrap_or(default)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        // `captures_dir` and `work_dir` are no longer settings; old configs
        // that carry them still parse. An unusable `archive_dir` falls back
        // to the default.
        for key in ["vault_id", "lifecycle"] {
            if self.document.get(key).is_some_and(|v| v.as_str().is_none()) {
                return Err(ConfigError::Field(key.into()));
            }
        }
        if !matches!(self.lifecycle(), "on" | "off") {
            return Err(ConfigError::Field("lifecycle".into()));
        }
        if let Some(id) = self.vault_id() {
            let bytes = id.as_bytes();
            let valid = bytes.len() == 36
                && bytes.iter().enumerate().all(|(i, c)| {
                    if [8, 13, 18, 23].contains(&i) {
                        *c == b'-'
                    } else {
                        c.is_ascii_hexdigit()
                    }
                })
                && bytes[14] == b'4'
                && matches!(bytes[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B');
            if !valid {
                return Err(ConfigError::Field("vault_id must be UUIDv4".into()));
            }
        }
        if self
            .document
            .get("stale_after_days")
            .is_some_and(|v| v.as_integer().is_none_or(|n| n < 0))
        {
            return Err(ConfigError::Field("stale_after_days".into()));
        }
        if self
            .document
            .get("max_note_bytes")
            .is_some_and(|v| v.as_integer().is_none_or(|n| n <= 0))
        {
            return Err(ConfigError::Field("max_note_bytes".into()));
        }
        Ok(())
    }
}

/// A relative directory inside the vault that names no hidden or parent
/// folder, written with `/` separators.
fn safe_dir(path: &str) -> bool {
    let trimmed = path.trim_matches('/');
    !path.starts_with('/')
        && !path.contains(['\\', '\0', ':'])
        && !trimmed.is_empty()
        && trimmed
            .split('/')
            .all(|s| !s.is_empty() && s != ".." && !s.starts_with('.'))
}
