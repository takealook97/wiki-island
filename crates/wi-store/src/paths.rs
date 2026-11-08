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
#[cfg(not(windows))]
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

/// Turn user input (vault-relative, or absolute inside the vault) into a
/// normalized relative path on Windows (D8). Drive (`C:\`), verbatim
/// (`\\?\C:\`) and UNC prefixes are parsed first and the vault root is
/// stripped component by component, case-insensitively; drive-relative
/// (`C:foo`), root-relative (`\foo`) and device (`\\.\`) inputs are
/// refused. `\` and `/` both separate components, every component must be
/// a name Windows can store (D7), and the result always uses `/`.
#[cfg(windows)]
pub(crate) fn normalize(root: &Path, input: &str) -> Result<String> {
    let rel = normalize_syntax(root, input)?;
    for part in rel.split('/') {
        check_windows_name(part).map_err(|reason| {
            StoreError::Invalid(format!("{input:?} is not a vault path ({reason})"))
        })?;
    }
    Ok(rel)
}

/// [`normalize`] without the Windows name rules (D7): only prefixes,
/// separators and traversal.
#[cfg(windows)]
fn normalize_syntax(root: &Path, input: &str) -> Result<String> {
    use std::path::Prefix;
    if input.is_empty() || input.chars().any(|c| c.is_control()) {
        return Err(StoreError::Invalid(format!(
            "{input:?} is not a vault path (empty or control character)"
        )));
    }
    let path = Path::new(input);
    let mut components = path.components().peekable();
    let relative: Vec<Component> = match components.peek() {
        Some(Component::Prefix(prefix)) => {
            match prefix.kind() {
                Prefix::DeviceNS(_) | Prefix::Verbatim(_) => {
                    return Err(protected(input, "device path"));
                }
                Prefix::Disk(_) | Prefix::VerbatimDisk(_) => {
                    if !path.has_root() {
                        return Err(protected(input, "drive-relative path"));
                    }
                }
                Prefix::UNC(..) | Prefix::VerbatimUNC(..) => {}
            }
            strip_root_windows(root, path).ok_or_else(|| protected(input, "outside the vault"))?
        }
        Some(Component::RootDir) => return Err(protected(input, "root-relative path")),
        _ => components.collect(),
    };
    let mut parts = Vec::new();
    for component in relative {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| StoreError::Invalid(format!("{input:?} is not UTF-8")))?;
                // A verbatim prefix turns off `/` parsing for the rest.
                for piece in part.split('/') {
                    match piece {
                        "" | "." => {}
                        ".." => return Err(protected(input, "outside the vault")),
                        piece => parts.push(piece.to_owned()),
                    }
                }
            }
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

/// One component of a path prefix, folded for comparison: a drive letter
/// or UNC server/share in upper case, verbatim or not.
#[cfg(windows)]
fn prefix_key(component: &Component) -> Option<String> {
    use std::path::Prefix;
    match component {
        Component::Prefix(prefix) => Some(match prefix.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                format!("{}:", char::from(d).to_ascii_uppercase())
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
                "\\\\{}\\{}",
                server.to_string_lossy().to_uppercase(),
                share.to_string_lossy().to_uppercase()
            ),
            _ => return None,
        }),
        Component::RootDir => Some("\\".to_owned()),
        Component::Normal(part) => Some(part.to_string_lossy().to_lowercase()),
        Component::CurDir | Component::ParentDir => None,
    }
}

/// Windows [`strip_root`]: the vault root is stripped component by
/// component, case-insensitively (a verbatim and a plain drive prefix
/// match); otherwise the shallowest prefix of `path` whose canonical form
/// is the root is stripped. A lookalike (`C:\vault2` for `C:\vault`)
/// never matches because whole components are compared.
#[cfg(windows)]
fn strip_root_windows<'a>(root: &Path, path: &'a Path) -> Option<Vec<Component<'a>>> {
    let components: Vec<Component<'a>> = path.components().collect();
    let root_keys: Option<Vec<String>> = root.components().map(|c| prefix_key(&c)).collect();
    if let Some(root_keys) = root_keys
        && components.len() >= root_keys.len()
        && components
            .iter()
            .zip(&root_keys)
            .all(|(c, key)| prefix_key(c).as_ref() == Some(key))
    {
        return Some(components[root_keys.len()..].to_vec());
    }
    let mut prefix = PathBuf::new();
    for (i, component) in components.iter().enumerate() {
        prefix.push(component.as_os_str());
        if matches!(component, Component::ParentDir | Component::Prefix(_)) {
            continue;
        }
        if canonical(&prefix).is_some_and(|c| c == root) {
            return Some(components[i + 1..].to_vec());
        }
    }
    None
}

/// `std::fs::canonicalize`, on Windows without the verbatim prefix the
/// vault root is stored without (`\\?\C:\x` → `C:\x`,
/// `\\?\UNC\s\x` → `\\s\x`).
#[cfg(not(windows))]
pub(crate) fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// `std::fs::canonicalize`, on Windows without the verbatim prefix the
/// vault root is stored without (`\\?\C:\x` → `C:\x`,
/// `\\?\UNC\s\x` → `\\s\x`).
#[cfg(windows)]
pub(crate) fn canonical(path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let text = canonical.to_str()?;
    Some(if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
        PathBuf::from(format!("\\\\{rest}"))
    } else if let Some(rest) = text.strip_prefix("\\\\?\\") {
        PathBuf::from(rest)
    } else {
        canonical
    })
}

/// Why `name` cannot be a component of a vault path on Windows (D7): a
/// character Windows reserves, a reserved device name (with any
/// extension), or a trailing dot or space.
pub(crate) fn check_windows_name(name: &str) -> std::result::Result<(), &'static str> {
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*' | '\\'))
    {
        return Err("a character Windows does not allow in names");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("a trailing dot or space");
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let upper = stem.to_uppercase();
    let reserved = matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|device| {
        upper.strip_prefix(device).is_some_and(|n| {
            let mut chars = n.chars();
            matches!(
                (chars.next(), chars.next()),
                (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
            )
        })
    });
    if reserved {
        return Err("a reserved device name");
    }
    Ok(())
}

/// A path relative to the vault root as a vault path string (`/`
/// separators on every platform); `None` when it is not UTF-8.
pub(crate) fn rel_string(rel: &Path) -> Option<String> {
    let text = rel.to_str()?;
    Some(if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.to_owned()
    })
}

/// Whether this device can store the vault path `rel` by name: always on
/// Unix; on Windows no component may contain `\\` or break the D7 name
/// rules. Such paths stay in history and are skipped here.
pub(crate) fn host_can_name(rel: &str) -> bool {
    !cfg!(windows) || rel.split('/').all(|part| check_windows_name(part).is_ok())
}

/// The part of an absolute `path` below the canonical `root`. Only a prefix
/// of `path` is canonicalized (the shallowest one that resolves to the
/// root, so `/var` → `/private/var` style system aliases work); the
/// components after it are returned as given, so an in-vault symlink stays
/// visible to the symlink check instead of being resolved away.
#[cfg(not(windows))]
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

/// Whether `meta` (not followed) is a symlink; on Windows any reparse
/// point (junction, symlink, mount point, cloud placeholder) counts.
fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        /// `FILE_ATTRIBUTE_REPARSE_POINT`.
        const REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & REPARSE_POINT != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

/// Refuse a path any existing component of which is a symlink.
pub(crate) fn check_no_symlink(root: &Path, rel: &str) -> Result<()> {
    let mut current = root.to_path_buf();
    for part in rel.split('/') {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if is_link(&meta) => {
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
        #[cfg(not(windows))]
        assert!(normalize(root, "a\\b.md").is_err());
        #[cfg(windows)]
        assert_eq!(normalize(root, "a\\b.md").unwrap(), "a/b.md");
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

    #[test]
    fn windows_name_rules_refuse_reserved_names_characters_and_trailers() {
        for bad in [
            "CON",
            "con.md",
            "Con .md",
            "PRN.txt",
            "aux",
            "NUL.tar.gz",
            "COM0",
            "com1.md",
            "COM9",
            "LPT1",
            "lpt9.txt",
            "COM\u{b9}",
            "lpt\u{b2}.md",
            "COM\u{b3}.x",
            "CONIN$",
            "a:b",
            "a<b",
            "a>b",
            "a\"b",
            "a|b",
            "a?b",
            "a*b",
            "a\\b",
            "a\u{1}b",
            "x.",
            "x ",
            "x.md.",
        ] {
            assert!(check_windows_name(bad).is_err(), "{bad:?}");
        }
        for good in [
            "console.md",
            "COM10",
            "LPT",
            "nul-x.md",
            "auxiliary",
            "a.b",
            ".hidden",
            "caf\u{e9}",
            "COM\u{b9}0",
        ] {
            assert!(check_windows_name(good).is_ok(), "{good:?}");
        }
        assert!(host_can_name("a/b.md"));
        assert_eq!(host_can_name("a/CON.md"), !cfg!(windows));
    }

    #[cfg(windows)]
    #[test]
    fn windows_input_forms_normalize_against_the_vault_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path()).unwrap();
        let text = root.to_str().unwrap().to_owned();
        assert!(!text.starts_with("\\\\?\\"), "{text}");
        let ok = |input: &str, want: &str| {
            assert_eq!(normalize(&root, input).unwrap(), want, "{input}");
        };
        ok("a\\b.md", "a/b.md");
        ok("a/b\\c.md", "a/b/c.md");
        ok(&format!("{text}\\a\\b.md"), "a/b.md");
        ok(&format!("{text}/a/b.md"), "a/b.md");
        ok(&format!("\\\\?\\{text}\\a.md"), "a.md");
        ok(&format!("{}\\a.md", text.to_uppercase()), "a.md");
        ok(&format!("{}\\a.md", text.to_lowercase()), "a.md");
        let refused = |input: &str| {
            assert!(normalize(&root, input).is_err(), "{input}");
        };
        refused("C:a.md");
        refused("\\a.md");
        refused("/a.md");
        refused("\\\\.\\C:\\a.md");
        refused("\\\\?\\GLOBALROOT\\Device\\x");
        refused("\\\\server\\share\\a.md");
        refused(&format!("{text}2\\a.md"));
        refused(&format!("{text}\\..\\a.md"));
        refused("a\\CON.md");
        refused("a\\b.md.");
        refused("a:b.md");
        refused(&text);
    }
}
