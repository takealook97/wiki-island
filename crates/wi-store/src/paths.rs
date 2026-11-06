//! Target resolution and protected paths (OPERATIONS write path step 1).
//!
//! Ordinary tools refuse the vault config and the engine directory, the
//! generated export paths named in config, anything under `.git`, paths
//! outside the vault, engine scratch files, and every path with a symlink
//! component (OPERATIONS `#path-identity-and-containment`).
//!
//! Protected names are compared by file-system identity, not by bytes: each
//! component is folded (Unicode NFC, then lowercase, then NFC) on every
//! platform, so `.GIT`, `.Wiki-Island`, `Exports` or a decomposed `cafe\u{301}`
//! match what a case- and normalization-insensitive volume (APFS) would open.
//! An absolute input only has its vault-root prefix canonicalized; the
//! components inside the vault are checked as given, so an in-vault symlink
//! is refused rather than resolved. File access then goes through
//! [`crate::contained::Dir`], which opens each directory without following
//! symlinks.

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;
use wi_note::VaultConfig;

use crate::error::{Result, StoreError};

/// Marker in the name of an engine temp file: `.<name>.wi-tmp-<pid>-<n>`.
pub(crate) const TMP_MARK: &str = ".wi-tmp-";
/// Marker in the name of a conflict copy: `.<name>.wi-conflict-<seq>`.
pub(crate) const CONFLICT_MARK: &str = ".wi-conflict-";
/// Marker of a move source taken aside: `.<name>.wi-moved-<seq>`.
pub(crate) const MOVED_MARK: &str = ".wi-moved-";
/// Config key listing generated export paths, protected from tools.
pub const EXPORT_PATHS_KEY: &str = "export_paths";
/// Hidden folder of the generated host exports (still synced with the vault).
pub const EXPORTS_DIR: &str = ".wiki-island/exports";
/// Hidden folder of the records importers keep between runs (synced with
/// the vault, never among the owner's notes).
pub const IMPORTS_DIR: &str = ".wiki-island/imports";
/// Each host, its default export path and the visible path older versions
/// wrote. The default paths are the only files under `.wiki-island/` the
/// export writer may create; ordinary tools still refuse them.
pub(crate) const HOST_EXPORTS: [(&str, &str, &str); 3] = [
    (
        "codex",
        ".wiki-island/exports/AGENTS.md",
        "exports/AGENTS.md",
    ),
    (
        "claude",
        ".wiki-island/exports/CLAUDE.md",
        "exports/CLAUDE.md",
    ),
    (
        "gemini",
        ".wiki-island/exports/GEMINI.md",
        "exports/GEMINI.md",
    ),
];

/// A resolved vault-relative target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Vault-relative path with `/` separators.
    pub rel: String,
    /// Absolute path under the canonical vault root.
    pub abs: PathBuf,
}

impl Target {
    pub(crate) fn file_name(&self) -> &str {
        self.rel.rsplit('/').next().unwrap_or(&self.rel)
    }

    /// Name of an engine scratch sibling: `.<name><mark><suffix>`.
    pub(crate) fn sibling_name(&self, mark: &str, suffix: &str) -> String {
        format!(".{}{mark}{suffix}", self.file_name())
    }

    pub(crate) fn rel_sibling(&self, mark: &str, suffix: &str) -> String {
        let name = format!(".{}{mark}{suffix}", self.file_name());
        match self.rel.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{name}"),
            None => name,
        }
    }
}

/// Whether a file name is an engine scratch file (temp, conflict copy or
/// moved-aside source). Watchers and scans skip them.
pub fn is_scratch_name(name: &str) -> bool {
    let name = fold(name);
    name.starts_with('.')
        && (name.contains(TMP_MARK) || name.contains(CONFLICT_MARK) || name.contains(MOVED_MARK))
}

/// A path component folded for identity comparison: NFC, lowercase, NFC.
/// Conservative on case-sensitive volumes (it may refuse more names there).
pub(crate) fn fold(name: &str) -> String {
    let composed: String = name.nfc().collect();
    composed.to_lowercase().nfc().collect()
}

fn folded_parts(rel: &str) -> Vec<String> {
    rel.split('/').map(fold).collect()
}

/// Whether `parts` starts with the components of `prefix` (both folded).
fn has_prefix(parts: &[String], prefix: &[String]) -> bool {
    !prefix.is_empty() && parts.len() >= prefix.len() && parts[..prefix.len()] == *prefix
}

/// Top-level directories of other applications that share the vault
/// folder (Obsidian settings and its trash). The engine never reads,
/// indexes, commits or modifies anything below them.
pub const FOREIGN_DIRS: &[&str] = &[".obsidian", ".trash"];

/// Paths the engine itself owns, never ingested as notes.
pub(crate) fn is_engine_internal(rel: &str) -> bool {
    let parts = folded_parts(rel);
    parts.first().is_some_and(|p| p == ".git")
        || parts
            .first()
            .is_some_and(|p| FOREIGN_DIRS.contains(&p.as_str()))
        || has_prefix(&parts, &[".wiki-island".into(), "local".into()])
        || parts
            .first()
            .is_some_and(|p| p.starts_with(".wiki-island-init.tmp"))
        || rel.rsplit('/').next().is_some_and(is_scratch_name)
}

/// Whether `rel` lies under the vault directory `dir` (folded comparison).
pub(crate) fn is_under(rel: &str, dir: &str) -> bool {
    let dir = folded_parts(dir.trim_matches('/'));
    !dir.is_empty() && has_prefix(&folded_parts(rel), &dir)
}

/// Engine-owned executable state lives under the work directory: ordinary
/// tools may not create, change or move anything there, nor in the work
/// directory of a vault written before work notes moved under
/// `.wiki-island/` (its notes may still wait for migration).
pub(crate) fn check_engine_dir(config: &VaultConfig, rel: &str) -> Result<()> {
    if is_under(rel, config.work_dir()) || is_under(rel, &config.legacy_work_dir()) {
        return Err(protected(rel, "engine-owned work directory"));
    }
    Ok(())
}

/// Folders under `.wiki-island/` whose files the engine itself writes as
/// notes: work items, captures and built-in procedures.
pub(crate) fn is_managed(rel: &str) -> bool {
    [
        wi_note::WORK_DIR,
        wi_note::CAPTURES_DIR,
        wi_note::PROCEDURES_DIR,
    ]
    .iter()
    .any(|dir| is_under(rel, dir))
}

/// [`check_protected`] for a path the engine chose: the managed folders
/// under `.wiki-island/` are allowed; everything else is checked as usual.
pub(crate) fn check_protected_managed(config: &VaultConfig, rel: &str) -> Result<()> {
    if is_managed(rel)
        && !folded_parts(rel).iter().any(|p| p == ".git")
        && !rel.rsplit('/').next().is_some_and(is_scratch_name)
    {
        return Ok(());
    }
    check_protected(config, rel)
}

fn protected(rel: &str, reason: &'static str) -> StoreError {
    StoreError::Protected {
        path: rel.to_owned(),
        reason,
    }
}

/// Turn user input (vault-relative, or absolute inside the vault) into a
/// normalized relative path. Pure; no protection checks.
pub(crate) fn normalize(root: &Path, input: &str) -> Result<String> {
    if input.is_empty() || input.chars().any(|c| c.is_control()) || input.contains('\\') {
        return Err(StoreError::Invalid(format!(
            "{input:?} is not a vault path (empty, control character or backslash)"
        )));
    }
    let path = Path::new(input);
    let relative: PathBuf = if path.is_absolute() {
        strip_root(root, path).ok_or_else(|| protected(input, "outside the vault"))?
    } else {
        path.to_path_buf()
    };
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| StoreError::Invalid(format!("{input:?} is not UTF-8")))?
                    .to_owned(),
            ),
            Component::CurDir => {}
            Component::ParentDir => return Err(protected(input, "outside the vault")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(protected(input, "outside the vault"));
            }
        }
    }
    if parts.is_empty() {
        return Err(StoreError::Invalid("the vault root is not a note".into()));
    }
    Ok(parts.join("/"))
}

/// The part of an absolute `path` below the canonical `root`. Only a prefix
/// of `path` is canonicalized (the shallowest one that resolves to the
/// root, so `/var` → `/private/var` style system aliases work); the
/// components after it are returned as given, so an in-vault symlink stays
/// visible to the symlink check instead of being resolved away.
fn strip_root(root: &Path, path: &Path) -> Option<PathBuf> {
    if let Ok(rest) = path.strip_prefix(root) {
        return Some(rest.to_path_buf());
    }
    let components: Vec<Component> = path.components().collect();
    let mut prefix = PathBuf::new();
    for (i, component) in components.iter().enumerate() {
        prefix.push(component.as_os_str());
        if matches!(component, Component::ParentDir) {
            continue;
        }
        if std::fs::canonicalize(&prefix).is_ok_and(|c| c == root) {
            return Some(components[i + 1..].iter().collect());
        }
    }
    None
}

/// Resolve `input` for an ordinary write: normalize, then refuse protected
/// targets and symlink components.
/// `managed` also allows the engine-managed folders (a path the engine
/// chose, such as a capture note).
pub(crate) fn resolve_writable_as(
    root: &Path,
    config: &VaultConfig,
    input: &str,
    managed: bool,
) -> Result<Target> {
    let rel = normalize(root, input)?;
    if managed {
        check_protected_managed(config, &rel)?;
    } else {
        check_protected(config, &rel)?;
    }
    let abs = root.join(&rel);
    check_no_symlink(root, &rel)?;
    Ok(Target { rel, abs })
}

/// Resolve an import record: a Markdown file under [`IMPORTS_DIR`], and
/// nothing else under `.wiki-island/`.
pub(crate) fn resolve_import_record(root: &Path, input: &str) -> Result<Target> {
    let rel = normalize(root, input)?;
    if !is_under(&rel, IMPORTS_DIR)
        || !is_markdown(&rel)
        || folded_parts(&rel).iter().any(|p| p == ".git")
        || rel.rsplit('/').next().is_some_and(is_scratch_name)
    {
        return Err(protected(&rel, "not an import record"));
    }
    check_no_symlink(root, &rel)?;
    Ok(Target {
        abs: root.join(&rel),
        rel,
    })
}

/// Resolve `input` for a read: normalize and refuse symlinks and `.git`.
pub(crate) fn resolve_readable(root: &Path, input: &str) -> Result<Target> {
    let rel = normalize(root, input)?;
    if folded_parts(&rel).iter().any(|p| p == ".git") {
        return Err(protected(&rel, "inside .git"));
    }
    check_no_symlink(root, &rel)?;
    Ok(Target {
        abs: root.join(&rel),
        rel,
    })
}

pub(crate) fn check_protected(config: &VaultConfig, rel: &str) -> Result<()> {
    let parts = folded_parts(rel);
    if parts.iter().any(|p| p == ".git") {
        return Err(protected(rel, "inside .git"));
    }
    let first = parts.first().map(String::as_str).unwrap_or_default();
    if parts.iter().any(|p| p == ".wiki-island") {
        return Err(protected(rel, "vault config and engine directory"));
    }
    if first.starts_with(".wiki-island-init.tmp") {
        return Err(protected(rel, "engine directory"));
    }
    if FOREIGN_DIRS.contains(&first) {
        return Err(protected(rel, "another application's folder"));
    }
    if rel.rsplit('/').next().is_some_and(is_scratch_name) {
        return Err(protected(rel, "engine scratch file"));
    }
    for export in export_paths(config) {
        if has_prefix(&parts, &folded_parts(&export)) {
            return Err(protected(rel, "generated export"));
        }
    }
    Ok(())
}

/// Generated export paths from the config key `export_paths`.
pub(crate) fn export_paths(config: &VaultConfig) -> Vec<String> {
    let mut paths: Vec<String> = config
        .get(EXPORT_PATHS_KEY)
        .and_then(|item| item.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim_matches('/').to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for (host, default, _) in HOST_EXPORTS {
        paths.push(default.to_owned());
        if let Some(path) = config
            .get(&format!("export_{host}_path"))
            .and_then(|v| v.as_str())
        {
            // Match the normalized paths ordinary operations compare against.
            paths.push(
                Path::new(path)
                    .components()
                    .filter_map(|part| match part {
                        Component::Normal(p) => p.to_str(),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("/"),
            );
        }
    }
    paths
}

/// Whether `rel` is one of the default hidden export files (folded).
pub(crate) fn is_default_export(rel: &str) -> bool {
    let parts = folded_parts(rel);
    HOST_EXPORTS
        .iter()
        .any(|(_, default, _)| parts == folded_parts(default))
}

/// Whether `rel` is a generated export (default or configured) that the
/// detectors must not compare with notes.
pub(crate) fn is_generated_export(config: &VaultConfig, rel: &str) -> bool {
    let parts = folded_parts(rel);
    export_paths(config)
        .iter()
        .any(|export| has_prefix(&parts, &folded_parts(export)))
}

/// Refuse a path any existing component of which is a symlink.
pub(crate) fn check_no_symlink(root: &Path, rel: &str) -> Result<()> {
    let mut current = root.to_path_buf();
    for part in rel.split('/') {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(protected(rel, "symlink"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(StoreError::io(&current, e)),
        }
    }
    Ok(())
}

pub(crate) fn is_markdown(rel: &str) -> bool {
    rel.rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("md"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_names_are_recognized() {
        assert!(is_scratch_name(".a.md.wi-tmp-1-2"));
        assert!(is_scratch_name(".a.md.wi-conflict-7"));
        assert!(!is_scratch_name("a.md"));
        assert!(is_markdown("x/y.MD"));
        assert!(!is_markdown("x/.md"));
    }

    #[test]
    fn normalize_refuses_traversal() {
        let root = Path::new("/vault");
        assert!(normalize(root, "../x.md").is_err());
        assert!(normalize(root, "a/../../x.md").is_err());
        assert_eq!(normalize(root, "./a//b.md").unwrap(), "a/b.md");
        assert!(normalize(root, "a\\b.md").is_err());
    }

    #[test]
    fn protected_names_compare_by_folded_identity() {
        let config = VaultConfig::parse("export_paths = [\"exports\", \"caf\u{e9}\"]\n").unwrap();
        for rel in [
            ".GIT/x.md",
            "a/.Git/x.md",
            ".Wiki-Island/x.md",
            ".WIKI-ISLAND",
            "notion-imports/x/.wiki-island/config.toml",
            "a/.Wiki-Island/b.png",
            "Exports/x.md",
            "EXPORTS",
            "cafe\u{301}/x.md",
            "CAFE\u{301}/x.md",
            "Caf\u{c9}/x.md",
            "a/.x.md.WI-TMP-1-2",
        ] {
            assert!(check_protected(&config, rel).is_err(), "{rel}");
        }
        for rel in [
            "exportsx/a.md",
            "notes/exports/a.md",
            "git/a.md",
            "cafe/a.md",
            "a/wiki-island/b.md",
            "a/.wiki-island.md",
        ] {
            assert!(check_protected(&config, rel).is_ok(), "{rel}");
        }
        assert!(is_engine_internal(".GIT/HEAD"));
        assert!(is_engine_internal(".Wiki-Island/Local/x"));
        assert!(is_engine_internal(".obsidian/workspace.json"));
        assert!(is_engine_internal(".Trash/old.md"));
        assert!(!is_engine_internal("notes/.obsidian.md"));
        for rel in [".obsidian/app.json", ".OBSIDIAN/x.md", ".trash/x.md"] {
            assert!(check_protected(&config, rel).is_err(), "{rel}");
        }
    }

    #[test]
    fn only_the_default_hidden_exports_are_export_files() {
        let plain = VaultConfig::parse("").unwrap();
        for rel in [
            ".wiki-island/exports/AGENTS.md",
            ".Wiki-Island/Exports/claude.md",
            ".wiki-island/exports/GEMINI.md",
        ] {
            assert!(is_default_export(rel), "{rel}");
            assert!(is_generated_export(&plain, rel), "{rel}");
            assert!(check_protected(&plain, rel).is_err(), "{rel}");
        }
        for rel in [
            ".wiki-island/exports/other.md",
            ".wiki-island/exports",
            ".wiki-island/config.toml",
            "exports/AGENTS.md",
        ] {
            assert!(!is_default_export(rel), "{rel}");
        }
        assert!(!is_generated_export(&plain, "notes/AGENTS.md"));
    }
}
