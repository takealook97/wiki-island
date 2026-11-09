//! Settings, walk and ignore rules.
use crate::{Error, Result};
use ignore::WalkBuilder;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use wi_note::VaultConfig;

use super::*;

pub(super) struct Settings {
    pub(super) ignored: Vec<PathBuf>,
    pub(super) max_note_bytes: u64,
}
pub(super) fn settings(root: &Path) -> Result<Settings> {
    let mut settings = Settings {
        ignored: vec![PathBuf::from(".wiki-island/local")],
        max_note_bytes: DEFAULT_MAX_NOTE_BYTES,
    };
    let path = root.join(wi_note::VAULT_CONFIG_PATH);
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(settings),
        Err(e) => return Err(e.into()),
    };
    let config = VaultConfig::parse(&text)?;
    settings.max_note_bytes = config.max_note_bytes();
    let invalid = || {
        Error::Config(wi_note::ConfigError::Field(
            "ignore_dirs must be an array of safe relative directories".into(),
        ))
    };
    if let Some(value) = config.get("ignore_dirs") {
        for item in value.as_array().ok_or_else(invalid)? {
            let path = PathBuf::from(item.as_str().ok_or_else(invalid)?);
            validate_relative(&path).map_err(|_| invalid())?;
            settings.ignored.push(path);
        }
    }
    Ok(settings)
}
pub(super) fn skip(
    report: &mut SyncReport,
    path: impl Into<PathBuf>,
    error: impl std::fmt::Display,
) {
    let path: PathBuf = path.into();
    // Diagnostic paths must remain serializable even for unsupported byte names.
    let path = PathBuf::from(path_key(&path));
    report.skipped.push(path.clone());
    report.diagnostics.push(IndexDiagnostic {
        rel_path: path,
        message: error.to_string(),
    });
}
/// Replace the stored `skipped` and `diagnostics` entries of the `affected`
/// paths with a targeted update's own, so search results stop reporting a
/// repaired file as skipped (and start reporting a newly broken one) before
/// the next full walk. Other paths keep their last-walk entries.
pub(super) fn refresh_walk_meta(
    tx: &rusqlite::Transaction<'_>,
    affected: &[String],
    skipped: &[PathBuf],
    diagnostics: &[IndexDiagnostic],
) -> Result<()> {
    use rusqlite::OptionalExtension;
    let touched: BTreeSet<&str> = affected.iter().map(String::as_str).collect();
    let is_touched = |p: &Path| touched.contains(path_key(p).as_str());
    let read = |key: &str| -> Result<Option<String>> {
        Ok(tx
            .query_row("SELECT value FROM index_meta WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    };
    let write = |key: &str, value: String| -> Result<()> {
        tx.execute(
            "INSERT INTO index_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [key, value.as_str()],
        )?;
        Ok(())
    };
    if let Some(stored) = read("skipped")? {
        let stored: Vec<PathBuf> = serde_json::from_str(&stored)?;
        let kept: Vec<PathBuf> = stored
            .into_iter()
            .filter(|p| !is_touched(p))
            .chain(skipped.iter().cloned())
            .collect();
        write("skipped", serde_json::to_string(&kept)?)?;
    }
    if let Some(stored) = read("diagnostics")? {
        let stored: Vec<IndexDiagnostic> = serde_json::from_str(&stored)?;
        let kept: Vec<IndexDiagnostic> = stored
            .into_iter()
            .filter(|d| !is_touched(&d.rel_path))
            .chain(diagnostics.iter().cloned())
            .collect();
        write("diagnostics", serde_json::to_string(&kept)?)?;
    }
    Ok(())
}
/// Top-level folders of other applications sharing the vault (Obsidian
/// settings and trash): never indexed.
pub(super) const FOREIGN_DIRS: &[&str] = &[".obsidian", ".trash"];

pub(super) fn reserved(path: &Path, ignored: &[PathBuf]) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
        || path.components().next().is_some_and(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|s| FOREIGN_DIRS.contains(&s.to_lowercase().as_str()))
        })
        || ignored.iter().any(|dir| path.starts_with(dir))
        || wi_note::is_unindexed_engine_path(&path_key(path))
}
pub(super) fn walk(
    root: &Path,
    ignored: Vec<PathBuf>,
    report: &mut SyncReport,
) -> BTreeMap<String, Stamp> {
    let root_owned = root.to_path_buf();
    let exclude = exclude_matcher(root);
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .require_git(false)
        .filter_entry(move |entry| {
            #[cfg(windows)]
            if fs::symlink_metadata(entry.path()).is_ok_and(|m| is_link(&m)) {
                return false;
            }
            entry
                .path()
                .strip_prefix(&root_owned)
                .is_ok_and(|rel| !reserved(rel, &ignored))
                && !exclude.as_ref().is_some_and(|m| {
                    walk_excluded(
                        &root_owned,
                        m,
                        entry.path(),
                        entry.file_type().is_some_and(|t| t.is_dir()),
                    )
                })
        });
    let mut files = BTreeMap::new();
    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                // An ignore walker error may hide a directory; retain old rows below it.
                skip(report, PathBuf::new(), e);
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) || !markdown(entry.path()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let checked = validate_relative(relative).and_then(|()| stamp(entry.path()));
        match checked {
            Ok(stamp) => {
                files.insert(path_key(relative), stamp);
            }
            Err(e) => skip(report, relative, e),
        }
    }
    files
}
pub(super) fn allowed(root: &Path, path: &Path, ignored: &[PathBuf]) -> Result<bool> {
    if reserved(path, ignored) || !markdown(path) {
        return Ok(false);
    }
    let mut ancestor = root.to_path_buf();
    for component in path.components() {
        ancestor.push(component);
        if fs::symlink_metadata(&ancestor).is_ok_and(|m| is_link(&m)) {
            return Err(Error::InvalidPath(path.into()));
        }
    }
    let (excluded, mut errors) = ignored_with_errors(root, path);
    if !errors.is_empty() {
        return Err(errors.swap_remove(0).into());
    }
    Ok(!excluded)
}

/// Extra exclude files (a folder-mode vault's `<gitdir>/info/exclude`),
/// keyed by canonical vault root. The vault registers its file on open.
pub(super) static EXCLUDES: std::sync::Mutex<BTreeMap<PathBuf, PathBuf>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Register (or clear) the git exclude file whose rules apply to the vault
/// at `root` below its `.ignore` and `.gitignore` files. A git-mode vault
/// keeps its rules in `.gitignore` and registers nothing.
pub fn set_exclude_file(root: &Path, file: Option<&Path>) {
    let key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut map = EXCLUDES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match file {
        Some(file) => {
            map.insert(key, file.to_path_buf());
        }
        None => {
            map.remove(&key);
        }
    }
}

/// The matcher of the exclude file registered for `root`, rooted at `root`.
pub fn exclude_matcher(root: &Path) -> Option<ignore::gitignore::Gitignore> {
    let file = {
        let map = EXCLUDES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(root)
            .or_else(|| root.canonicalize().ok().and_then(|c| map.get(&c)))
            .cloned()?
    };
    if !file.is_file() {
        return None;
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    builder.add(file);
    builder.build().ok()
}

/// Whether the `.ignore` and `.gitignore` files of `root` and of the
/// directories above `rel` exclude the vault-relative `rel`, decided the
/// way the vault walk decides: an excluded directory hides everything
/// below it, deeper files win over shallower ones, and `.ignore` wins over
/// `.gitignore`. Scans, file events and the index share this one rule.
/// Rules that parse apply even when a file is partly invalid.
pub fn ignored(root: &Path, rel: &Path) -> bool {
    ignored_with_errors(root, rel).0
}

/// [`ignored`] for a directory: the last component of `rel` is matched as
/// a directory too, so a `name/` pattern excludes it. Walkers ask this
/// before reading a directory, so an excluded (even unreadable) one is
/// pruned without being opened.
pub fn ignored_dir(root: &Path, rel: &Path) -> bool {
    ignored_kind(root, rel, true).0
}

/// [`ignored`], also returning the errors of unreadable or partly invalid
/// ignore files.
pub fn ignored_with_errors(root: &Path, rel: &Path) -> (bool, Vec<ignore::Error>) {
    ignored_kind(root, rel, false)
}

/// The shared rule; `dir` says whether the last component of `rel` is a
/// directory (every earlier component is one).
pub(super) fn ignored_kind(root: &Path, rel: &Path, dir: bool) -> (bool, Vec<ignore::Error>) {
    ignored_with(root, rel, dir, exclude_matcher(root).as_ref())
}

/// Whether a vault walk of `root` skips `path` (below `root`) for the
/// registered exclude file `exclude`. The exclude file has the lowest
/// priority, so a `.ignore` or `.gitignore` rule that re-includes the path
/// wins, as in [`ignored`]. Every vault walker uses this one decision.
pub fn walk_excluded(
    root: &Path,
    exclude: &ignore::gitignore::Gitignore,
    path: &Path,
    is_dir: bool,
) -> bool {
    if !exclude.matched(path, is_dir).is_ignore() {
        return false;
    }
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    ignored_with(root, rel, is_dir, Some(exclude)).0
}

pub(super) fn ignored_with(
    root: &Path,
    rel: &Path,
    dir: bool,
    exclude: Option<&ignore::gitignore::Gitignore>,
) -> (bool, Vec<ignore::Error>) {
    use ignore::{Match, gitignore::Gitignore};
    let mut errors = Vec::new();
    let mut load = |dir: &Path, name: &str| -> Option<Gitignore> {
        let file = dir.join(name);
        if !file.is_file() {
            return None;
        }
        let mut builder = ignore::gitignore::GitignoreBuilder::new(dir);
        if let Some(error) = builder.add(file) {
            errors.push(error);
        }
        match builder.build() {
            Ok(matcher) => Some(matcher),
            Err(error) => {
                errors.push(error);
                None
            }
        }
    };
    let parts: Vec<_> = rel.components().collect();
    // Matchers of the directories from the root down, by file kind.
    let mut dot_ignore: Vec<Gitignore> = Vec::new();
    let mut git_ignore: Vec<Gitignore> = Vec::new();
    let mut path = root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        dot_ignore.extend(load(&path, ".ignore"));
        git_ignore.extend(load(&path, ".gitignore"));
        path.push(part);
        let is_dir = dir || i + 1 < parts.len();
        let decide = |matchers: &[Gitignore]| {
            matchers
                .iter()
                .rev()
                .find_map(|m| match m.matched(&path, is_dir) {
                    Match::None => None,
                    found => Some(found.is_ignore()),
                })
        };
        if decide(&dot_ignore)
            .or_else(|| decide(&git_ignore))
            .or_else(|| decide(exclude.map_or(&[], std::slice::from_ref)))
            == Some(true)
        {
            return (true, errors);
        }
    }
    (false, errors)
}
