//! Locating and reading the conversation files of an export (ZIP, folder or
//! single JSON file). Only the conversation JSON files and `projects.json`
//! are read; images and HTML never are, and do not count toward the limit.
use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::Path;

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde_json::Value;
use wi_store::Vault;

use super::refuse;
use crate::notion::{
    MAX_DEPTH, MAX_ENTRIES, bounded_read, checked_path, is_zip64, source_path, zip_preflight_file,
};
use crate::{Error, MAX_SOURCE_BYTES, Result, source_input};

/// Directory entries examined in one folder before it is refused as too large.
const MAX_LISTING: usize = 1_000_000;

/// Safety limits; injectable so tests need no huge files.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub max_json_bytes: u64,
    /// Conversation and kind-detection files read from one folder.
    pub max_entries: usize,
    /// Entries of one folder that may be looked through by name.
    pub max_listing: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_json_bytes: MAX_SOURCE_BYTES,
            max_entries: MAX_ENTRIES,
            max_listing: MAX_LISTING,
        }
    }
}

pub(crate) const PROJECTS: &str = "projects.json";
const CLAUDE_HINTS: [&str; 2] = ["projects.json", "users.json"];
const CHATGPT_HINTS: [&str; 2] = ["chat.html", "user.json"];

/// The files read from an export, keyed by file name within its directory.
#[derive(Debug, Default)]
pub(crate) struct Source {
    /// Conversation files in name order.
    pub conversations: Vec<(String, Vec<u8>)>,
    pub projects: Option<Vec<u8>>,
    /// Every file name beside the conversation files (not read).
    pub siblings: Vec<String>,
    /// A single JSON file given directly rather than an export.
    pub single: bool,
}

impl Source {
    pub(crate) fn json_bytes(&self) -> impl Iterator<Item = &[u8]> {
        self.conversations
            .iter()
            .map(|(_, b)| b.as_slice())
            .chain(self.projects.as_deref())
    }

    pub(crate) fn has(&self, names: &[&str]) -> bool {
        self.siblings.iter().any(|s| names.contains(&s.as_str()))
    }

    pub(crate) fn claude_hint(&self) -> bool {
        self.projects.is_some() || self.has(&CLAUDE_HINTS)
    }

    pub(crate) fn chatgpt_hint(&self) -> bool {
        self.has(&CHATGPT_HINTS)
    }
}

fn conversation_file(name: &str) -> bool {
    name == "conversations.json"
        || (name.starts_with("conversations-") && name.ends_with(".json") && name.len() > 19)
}

fn wanted(name: &str) -> bool {
    conversation_file(name) || name == PROJECTS
}

/// Where an oversized export came from decides which next step is true.
#[derive(Clone, Copy, PartialEq)]
enum Origin {
    Zip,
    Other,
}

fn too_large(limit: u64, origin: Origin) -> Error {
    let mib = limit / (1024 * 1024);
    let split = match origin {
        Origin::Zip => {
            "If the export has split conversations-000.json files, unpack the archive and import them one by one."
        }
        Origin::Other => {
            "If the export has split conversations-000.json files, import them one by one."
        }
    };
    refuse(format!(
        "The conversation files in this export are larger than {mib} MiB. Export a smaller date range. {split}"
    ))
}

fn too_many_files(limit: usize) -> Error {
    refuse(format!(
        "This folder holds more than {limit} conversation files. Copy the files you want into a new folder and import that folder."
    ))
}

fn too_big_a_folder(limit: usize) -> Error {
    refuse(format!(
        "This folder holds more than {limit} files, too many to look through. Put the export in a folder of its own and import that folder."
    ))
}

fn unreadable_archive() -> Error {
    refuse(
        "This archive could not be read and may be incomplete. Download the export again and import the new file.",
    )
}

fn zip64() -> Error {
    refuse(
        "This archive is too big to be opened directly. Unpack it, then import the unpacked folder.",
    )
}

/// Refuse paths that leave the archive; deeper checks apply to read files.
fn structural(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', '\0'])
        || path.split('/').any(|p| p == ".." || p.is_empty())
        || path.split('/').count() > MAX_DEPTH
        || (path.len() > 1 && path.as_bytes()[1] == b':')
    {
        return Err(refuse(
            "This archive contains a file path that leaves the archive, so it was not opened. Download the export again and import the new file.",
        ));
    }
    Ok(())
}

/// Directory (`""` for the root) whose files hold the conversations: the root,
/// else the first directory one level below it.
fn locate(names: &[String]) -> Option<String> {
    let in_dir = |dir: &str| {
        names.iter().any(|n| {
            let (d, f) = n.rsplit_once('/').unwrap_or(("", n));
            d == dir && conversation_file(f) && (dir.is_empty() || !dir.contains('/'))
        })
    };
    if in_dir("") {
        return Some(String::new());
    }
    let mut dirs: Vec<&str> = names
        .iter()
        .filter_map(|n| n.split_once('/').map(|(d, _)| d))
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs.into_iter().find(|d| in_dir(d)).map(str::to_owned)
}

fn file_of<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    let name = if dir.is_empty() {
        path
    } else {
        path.strip_prefix(dir)?.strip_prefix('/')?
    };
    (!name.contains('/')).then_some(name)
}

/// Read `source` when it can hold conversations. `Ok(None)` means it is not
/// a conversation source and the caller should carry on as before.
pub(crate) fn load(vault: &Vault, source: &str, limits: &Limits) -> Result<Option<Source>> {
    let input = source_input(source);
    // A folder is a folder whatever its name ends with.
    if let Some(full) = source_path(vault.root(), input.as_ref())
        .ok()
        .filter(|p| p.is_dir())
    {
        return load_dir(&full, limits);
    }
    let ext = Path::new(input.as_ref())
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("zip") => load_zip(vault, input.as_ref(), limits),
        Some("json") => load_json(vault, input.as_ref(), limits),
        _ => Ok(None),
    }
}

fn load_json(vault: &Vault, source: &str, limits: &Limits) -> Result<Option<Source>> {
    let full = source_path(vault.root(), source)?;
    if !full.is_file() {
        return Ok(None);
    }
    if std::fs::metadata(&full)?.len() > limits.max_json_bytes {
        return Err(too_large(limits.max_json_bytes, Origin::Other));
    }
    let bytes = read_limited(
        std::fs::File::open(&full)?,
        limits.max_json_bytes,
        limits,
        Origin::Other,
    )?;
    let name = full
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("conversations.json")
        .to_owned();
    Ok(Some(Source {
        conversations: vec![(name.clone(), bytes)],
        siblings: sibling_hints(&full, &name, limits),
        single: true,
        ..Source::default()
    }))
}

/// Names beside a single file that only tell which service made the export.
fn sibling_hints(file: &Path, own: &str, limits: &Limits) -> Vec<String> {
    let Some(parent) = file.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .take(limits.max_listing)
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| n != own && is_hint(n))
        .collect()
}

/// Reads at most `remaining + 1` bytes, whatever the entry declares.
fn read_limited(
    reader: impl std::io::Read,
    remaining: u64,
    limits: &Limits,
    origin: Origin,
) -> Result<Vec<u8>> {
    bounded_read(reader, remaining as usize).map_err(|_| too_large(limits.max_json_bytes, origin))
}

fn is_hint(name: &str) -> bool {
    CLAUDE_HINTS.contains(&name) || CHATGPT_HINTS.contains(&name)
}

/// Names of one folder, refusing a listing past the bound instead of cutting it.
fn list_dir(dir: &Path, limits: &Limits) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        if entries.len() >= limits.max_listing {
            return Err(too_big_a_folder(limits.max_listing));
        }
        entries.push(entry?);
    }
    Ok(entries)
}

fn load_dir(full: &Path, limits: &Limits) -> Result<Option<Source>> {
    let Some(dir) = dir_with_conversations(full, limits)? else {
        return Ok(None);
    };
    let base = if dir.is_empty() {
        full.to_path_buf()
    } else {
        full.join(&dir)
    };
    // Select by name first: unrelated files are neither counted nor opened.
    let mut selected: Vec<_> = list_dir(&base, limits)?
        .into_iter()
        .filter_map(|e| {
            let name = e.file_name().to_str().map(str::to_owned)?;
            (wanted(&name) || is_hint(&name)).then_some((name, e))
        })
        .collect();
    if selected.len() > limits.max_entries {
        return Err(too_many_files(limits.max_entries));
    }
    selected.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Source::default();
    let mut remaining = limits.max_json_bytes;
    for (name, entry) in selected {
        if !wanted(&name) {
            out.siblings.push(name);
            continue;
        }
        if !entry.file_type()?.is_file() {
            return Err(refuse(
                "A conversation file in this folder is a link or not a regular file, so it was not opened. Copy the export into a new folder as ordinary files and import that folder.",
            ));
        }
        if entry.metadata()?.len() > remaining {
            return Err(too_large(limits.max_json_bytes, Origin::Other));
        }
        let bytes = read_limited(
            std::fs::File::open(entry.path())?,
            remaining,
            limits,
            Origin::Other,
        )?;
        remaining -= bytes.len() as u64;
        out.add(name, bytes);
    }
    Ok(Some(out))
}

/// Root, or the first directory one level below it, holding conversation files.
fn dir_with_conversations(full: &Path, limits: &Limits) -> Result<Option<String>> {
    let mut names = Vec::new();
    let mut dirs = Vec::new();
    for entry in list_dir(full, limits)? {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let kind = entry.file_type()?;
        if kind.is_dir() {
            dirs.push(name);
        } else if kind.is_file() || kind.is_symlink() {
            names.push(name);
        }
    }
    if names.iter().any(|n| conversation_file(n)) {
        return Ok(Some(String::new()));
    }
    dirs.sort();
    for dir in dirs {
        let inner = match list_dir(&full.join(&dir), limits) {
            Ok(inner) => inner,
            Err(error) if super::owner_sentence(&error).is_some() => return Err(error),
            Err(_) => continue,
        };
        let found = inner
            .iter()
            .any(|e| e.file_name().to_str().is_some_and(conversation_file));
        if found {
            return Ok(Some(dir));
        }
    }
    Ok(None)
}

fn load_zip(vault: &Vault, source: &str, limits: &Limits) -> Result<Option<Source>> {
    let full = source_path(vault.root(), source)?;
    // The archive stays on disk: only its directory and the entries that are
    // needed are read, so images and other large entries cost nothing.
    let mut file = std::fs::File::open(&full)?;
    zip_preflight_file(&mut file).map_err(|e| match e {
        e if is_zip64(&e) => zip64(),
        Error::Invalid(_) => unreadable_archive(),
        e => e,
    })?;
    let mut archive =
        zip::ZipArchive::new(BufReader::new(file)).map_err(|_| unreadable_archive())?;
    let mut names = Vec::new();
    let mut index_of = BTreeMap::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|_| unreadable_archive())?;
        let name = entry.name().trim_end_matches('/').to_owned();
        structural(&name)?;
        if entry.unix_mode().is_some_and(|mode| {
            mode & 0o170000 != 0 && !matches!(mode & 0o170000, 0o100000 | 0o040000)
        }) {
            return Err(refuse(
                "This archive contains a link or special file, so it was not opened. Download the export again and import the new file.",
            ));
        }
        if !entry.is_dir() {
            index_of.insert(name.clone(), index);
            names.push(name);
        }
    }
    let Some(dir) = locate(&names) else {
        return Ok(None);
    };
    let mut out = Source::default();
    let mut remaining = limits.max_json_bytes;
    for name in &names {
        let Some(file) = file_of(name, &dir) else {
            continue;
        };
        if !wanted(file) {
            out.siblings.push(file.to_owned());
            continue;
        }
        checked_path(name)?;
        let mut entry = archive
            .by_index(index_of[name])
            .map_err(|_| unreadable_archive())?;
        if entry.size() > remaining {
            return Err(too_large(limits.max_json_bytes, Origin::Zip));
        }
        let bytes = read_limited(&mut entry, remaining, limits, Origin::Zip)?;
        remaining -= bytes.len() as u64;
        out.add(file.to_owned(), bytes);
    }
    out.conversations.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Some(out))
}

impl Source {
    fn add(&mut self, name: String, bytes: Vec<u8>) {
        if name == PROJECTS {
            self.projects = Some(bytes);
        } else {
            self.conversations.push((name, bytes));
        }
    }
}

/// Name-based check used when expanding folder arguments.
pub(crate) fn is_export(root: &Path, source: &str) -> bool {
    let input = source_input(source);
    let Ok(full) = source_path(root, input.as_ref()) else {
        return false;
    };
    full.is_dir()
        && match dir_with_conversations(&full, &Limits::default()) {
            Ok(dir) => dir.is_some(),
            // Too large to look through: import repeats the check and says so.
            Err(error) => super::owner_sentence(&error).is_some(),
        }
}

fn without_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)
}

fn unreadable(file: &str) -> Error {
    refuse(format!(
        "{file} is not a readable list of conversations. Export your data again and import the new file."
    ))
}

struct Each<'a, F> {
    each: &'a mut F,
    failure: &'a mut Option<Error>,
}

impl<'de, F: FnMut(Value) -> Result<bool>> Visitor<'de> for Each<'_, F> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a list")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while let Some(value) = seq.next_element::<Value>()? {
            match (self.each)(value) {
                Ok(true) => {}
                Ok(false) => return Err(de::Error::custom("stopped")),
                Err(e) => {
                    *self.failure = Some(e);
                    return Err(de::Error::custom("stopped"));
                }
            }
        }
        Ok(())
    }
}

/// Call `each` with one element of the top-level JSON array at a time, so a
/// conversation list is never held in memory as one value. `each` returns
/// `Ok(false)` to stop early.
pub(crate) fn for_each(
    file: &str,
    bytes: &[u8],
    mut each: impl FnMut(Value) -> Result<bool>,
) -> Result<()> {
    let mut failure = None;
    let mut stopped = false;
    let mut de = serde_json::Deserializer::from_slice(without_bom(bytes));
    let outcome = de.deserialize_seq(Each {
        each: &mut |value| {
            let keep = each(value)?;
            stopped = !keep;
            Ok(keep)
        },
        failure: &mut failure,
    });
    if let Some(error) = failure {
        return Err(error);
    }
    if stopped {
        return Ok(());
    }
    outcome
        .and_then(|()| de.end())
        .map_err(|_| unreadable(file))
}

/// The first element of a JSON array, when there is one.
pub(crate) fn first_element(file: &str, bytes: &[u8]) -> Result<Option<Value>> {
    let mut first = None;
    for_each(file, bytes, |value| {
        first = Some(value);
        Ok(false)
    })?;
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_file_names() {
        assert!(conversation_file("conversations.json"));
        assert!(conversation_file("conversations-000.json"));
        assert!(!conversation_file("conversations-.json"));
        assert!(!conversation_file("conversation.json"));
        assert!(!conversation_file("users.json"));
    }

    #[test]
    fn locates_root_then_one_level_below() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            locate(&names(&["conversations.json", "a/x.png"])),
            Some(String::new())
        );
        assert_eq!(
            locate(&names(&["export/conversations-000.json"])),
            Some("export".into())
        );
        assert_eq!(locate(&names(&["a/b/conversations.json"])), None);
        assert_eq!(locate(&names(&["notes.md"])), None);
    }

    #[test]
    fn elements_are_streamed_and_early_stop_is_clean() {
        let bytes = br#"[{"a":1},{"a":2},{"a":3}]"#;
        let mut seen = 0;
        for_each("f.json", bytes, |_| {
            seen += 1;
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen, 3);
        assert_eq!(first_element("f.json", bytes).unwrap().unwrap()["a"], 1);
        assert!(first_element("f.json", b"[]").unwrap().is_none());
        assert!(for_each("f.json", b"{}", |_| Ok(true)).is_err());
        assert!(for_each("f.json", b"[{", |_| Ok(true)).is_err());
    }
}
