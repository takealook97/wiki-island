//! Lifecycle notes as engine snapshots, listed from the device index and
//! parsed once per content hash.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use rayon::prelude::*;
use wi_history::hash_bytes;
use wi_lifecycle::{NoteSnapshot, Snapshot, Timestamp};

use crate::contained::{Dir, read_rel, split};
use crate::error::Result;
use crate::ingest::walk_files;
use crate::vault::Vault;

/// The publication a read snapshot was built for: (generation,
/// index_as_of).
type BuiltKey = (u64, Option<String>);

/// Parsed lifecycle notes of the latest listing, each under the hash it
/// was resolved for. One instance is shared by the handles of a vault on
/// the same index in a process ([`Cache::shared`]): they list the same
/// hashes, so a note resolved for a hash is the same whichever handle
/// resolved it.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    /// path → hash of the parsed bytes of the note `notes` holds for it
    hashes: BTreeMap<String, String>,
    /// The notes of the latest listing, sorted by path.
    notes: Snapshot,
}

impl Parsed {
    /// The cached snapshot of `path` when it was parsed from `hash`.
    fn hit(&self, path: &str, hash: &str) -> Option<&NoteSnapshot> {
        (self.hashes.get(path).map(String::as_str) == Some(hash))
            .then(|| self.notes.get(path))
            .flatten()
    }
}

/// Which handles share parsed notes: the same vault root, vault id and
/// device index. Handles on another index (a copy, a rebuild target) list
/// other hashes and stamps, so they keep their own.
type SharedKey = (PathBuf, String, Option<PathBuf>);

/// The parse caches of the vaults open in this process. An entry dies with
/// the last handle that uses it.
type Registry = Vec<(SharedKey, Weak<Mutex<Parsed>>)>;
static SHARED: Mutex<Registry> = Mutex::new(Vec::new());

/// A holder that panics leaves no broken invariant: the parse cache is
/// replaced whole and the registry only gains or drops entries, so a
/// poisoned lock is used as is.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Lifecycle notes and resolved evidence times of one vault handle.
///
/// The parsed notes are shared by the handles of a vault, so a process
/// holds them once, not once per handle. The read snapshot and the
/// evidence times belong to this handle.
#[derive(Debug, Default)]
pub(crate) struct Cache {
    parsed: Arc<Mutex<Parsed>>,
    /// The read snapshot and the publication it was built for. It shares
    /// the notes of the listing it was built from, so reads keep the view
    /// they started with after a later listing replaces the parsed notes.
    built: Option<(BuiltKey, Snapshot)>,
    /// (ref, source_hash, commit) → engine-recorded evidence time
    pub evidence_times: HashMap<(String, String, Option<String>), Option<Timestamp>>,
    /// Ignore materialized snapshots in the index (differential tests).
    /// Such a handle keeps a private parse cache.
    pub files_only: bool,
}

impl Cache {
    /// A cache sharing its parsed notes with the other handles of the
    /// vault at `root` with `vault_id` on the index at `index` in this
    /// process.
    pub(crate) fn shared(root: &Path, vault_id: &str, index: Option<PathBuf>) -> Self {
        let key = (root.to_path_buf(), vault_id.to_owned(), index);
        let mut registry = lock(&SHARED);
        registry.retain(|(_, weak)| weak.strong_count() > 0);
        let found = registry
            .iter()
            .find(|(k, _)| *k == key)
            .and_then(|(_, weak)| weak.upgrade());
        let parsed = found.unwrap_or_else(|| {
            let parsed = Arc::default();
            registry.push((key, Arc::downgrade(&parsed)));
            parsed
        });
        Self {
            parsed,
            ..Self::default()
        }
    }
}

/// One listed lifecycle note: its indexed hash and stamp, and the snapshot
/// the index materialized for that hash, if any. Without an index only the
/// path is known.
struct Listed {
    path: String,
    hash: Option<String>,
    stamp: Option<(String, u64)>,
    materialized: Option<String>,
}

impl Listed {
    fn parent(&self) -> Option<&str> {
        self.path.rsplit_once('/').map(|(d, _)| d)
    }
}

/// One resolved listing entry: its path, the hash it was parsed from, and
/// the parsed note, or `None` when the parse cache already holds the note
/// for that hash (resolved without a copy). Boxed: a listing holds one
/// entry per lifecycle note, and most are cache hits.
type Resolved = (String, String, Option<Box<NoteSnapshot>>);

/// Resolve one directory's listed notes: a cached snapshot of the indexed
/// hash; else the materialized snapshot when the file's stamp still equals
/// the indexed stamp (the stat-walk rule); else the file's current bytes.
/// The directory chain is opened once, without following symlinks.
fn resolve_group(root: &Path, group: Vec<Listed>, cached: &Parsed) -> Vec<Option<Resolved>> {
    let mut dir: Option<Option<Dir>> = None;
    group
        .into_iter()
        .map(|entry| {
            if let Some(hash) = &entry.hash
                && cached.hit(&entry.path, hash).is_some()
            {
                return Some((entry.path, hash.clone(), None));
            }
            let opened = dir.get_or_insert_with(|| {
                Dir::open_parent(root, &entry.path)
                    .ok()
                    .flatten()
                    .map(|(d, _)| d)
            });
            let (Some(d), Ok((_, name))) = (opened.as_ref(), split(&entry.path)) else {
                return None;
            };
            if let (Some(hash), Some(stamp), Some(text)) =
                (&entry.hash, &entry.stamp, &entry.materialized)
                && d.stat_regular(name).ok().flatten().as_ref() == Some(stamp)
                && let Some(snap) = NoteSnapshot::from_materialized(&entry.path, text)
            {
                return Some((entry.path, hash.clone(), Some(Box::new(snap))));
            }
            let Ok(Some(bytes)) = d.read_regular(name) else {
                return None;
            };
            let snap = NoteSnapshot::from_bytes(entry.path.as_str(), &bytes);
            let lifecycle = snap.lifecycle.lifecycle_type != wi_note::LifecycleType::None;
            if entry.hash.is_none() && !lifecycle {
                return None;
            }
            let key = entry.hash.unwrap_or_else(|| hash_bytes(&bytes));
            Some((entry.path, key, Some(Box::new(snap))))
        })
        .collect()
}

impl Vault {
    /// Every lifecycle note with its indexed hash and stamp. Uses the index
    /// after a complete walk; otherwise (no index, or one being rebuilt)
    /// walks the vault (each Markdown file is parsed to find the lifecycle
    /// notes).
    fn lifecycle_listing(&self) -> Vec<Listed> {
        let indexed = self.with_index(|index| Ok(index.lifecycle_rows()?));
        if let Ok(Some(rows)) = indexed {
            return rows
                .into_iter()
                .map(|r| Listed {
                    path: r.path,
                    hash: Some(r.content_hash),
                    stamp: Some((r.mtime_ns, r.size)),
                    materialized: None,
                })
                .collect();
        }
        walk_files(self.root())
            .into_iter()
            .filter(|p| p.ends_with(".md"))
            .map(|path| Listed {
                path,
                hash: None,
                stamp: None,
                materialized: None,
            })
            .collect()
    }

    /// Attach the index's materialized snapshot to each listed note the
    /// handle cache cannot serve, when the snapshot was parsed from the
    /// listed hash. Cache hits need no snapshot text.
    fn with_materialized(&self, listing: Vec<Listed>) -> Vec<Listed> {
        let parsed = {
            let cache = self.lifecycle_cache.borrow();
            if cache.files_only {
                return listing;
            }
            cache.parsed.clone()
        };
        let cache = lock(&parsed);
        let misses: Vec<&str> = listing
            .iter()
            .filter(|l| {
                l.hash
                    .as_ref()
                    .is_some_and(|h| cache.hit(&l.path, h).is_none())
            })
            .map(|l| l.path.as_str())
            .collect();
        drop(cache);
        if misses.is_empty() {
            return listing;
        }
        let Ok(mut found) = self.with_index(|index| {
            Ok(index.lifecycle_snapshots(wi_lifecycle::MATERIALIZED_FORMAT, &misses)?)
        }) else {
            return listing;
        };
        listing
            .into_iter()
            .map(|entry| match found.remove(&entry.path) {
                Some((hash, text)) if Some(&hash) == entry.hash.as_ref() => Listed {
                    materialized: Some(text),
                    ..entry
                },
                _ => entry,
            })
            .collect()
    }

    /// Use (`false`) or ignore (`true`) the snapshots materialized in the
    /// index. Ignoring them re-reads every uncached lifecycle note from its
    /// file, the behavior before they existed; differential tests compare
    /// the two.
    #[doc(hidden)]
    pub fn set_lifecycle_files_only(&self, files_only: bool) {
        // A private parse cache: notes parsed by handles that use the
        // materialized snapshots must not answer for this one.
        let mut cache = self.lifecycle_cache.borrow_mut();
        *cache = crate::lifecycle::Cache {
            files_only,
            ..Default::default()
        };
    }

    /// A fresh snapshot of one note from its current bytes (`None` when absent).
    pub(crate) fn note_snapshot(&self, path: &str) -> Result<Option<(NoteSnapshot, Vec<u8>)>> {
        let Some(bytes) = read_rel(self.root(), path)? else {
            return Ok(None);
        };
        Ok(Some((NoteSnapshot::from_bytes(path, &bytes), bytes)))
    }

    /// The lifecycle notes (memory, capture, work) that `keep` selects,
    /// sorted by path. Notes whose bytes changed since the index saw them
    /// are parsed from disk; unchanged ones come from the parse cache or
    /// the index. Only the selected notes are copied out of the cache.
    pub(crate) fn lifecycle_notes_where(
        &self,
        keep: impl Fn(&NoteSnapshot) -> bool,
    ) -> Result<Vec<NoteSnapshot>> {
        let parsed = self.refresh_lifecycle_notes()?;
        Ok(lock(&parsed)
            .notes
            .notes()
            .iter()
            .filter(|n| keep(n))
            .cloned()
            .collect())
    }

    /// Parse the lifecycle notes into the parse cache shared by this
    /// vault's handles, ahead of the first read. No read snapshot is kept:
    /// the first read builds one from the parsed notes without parsing, and
    /// an idle handle holds no copy of an older listing.
    pub fn warm_lifecycle_notes(&self) -> Result<()> {
        if !self.lifecycle_on() {
            return Ok(());
        }
        self.refresh_lifecycle_notes().map(drop)
    }

    /// Bring the parsed notes up to date with the current listing; returns
    /// them.
    fn refresh_lifecycle_notes(&self) -> Result<Arc<Mutex<Parsed>>> {
        let listing = self.with_materialized(self.lifecycle_listing());
        let parsed = self.lifecycle_cache.borrow().parsed.clone();
        let mut cache = lock(&parsed);
        // Resolved per parent directory, in parallel; output keeps listing
        // order. Opening checked directory chains per file and parsing YAML
        // dominated a cold read (perf-opt); the index now holds the parse.
        let root = self.root();
        let cached = &*cache;
        let mut groups: Vec<Vec<Listed>> = Vec::new();
        for entry in listing {
            match groups.last_mut() {
                Some(group) if group[0].parent() == entry.parent() => group.push(entry),
                _ => groups.push(vec![entry]),
            }
        }
        let resolved: Vec<Resolved> = groups
            .into_par_iter()
            .flat_map_iter(|group| resolve_group(root, group, cached))
            .flatten()
            .collect();
        let mut hashes = BTreeMap::new();
        let mut kept = Vec::with_capacity(resolved.len());
        for (path, key, snap) in resolved {
            // The first entry of a path wins, as in `Snapshot::new`.
            if let std::collections::btree_map::Entry::Vacant(slot) = hashes.entry(path.clone()) {
                slot.insert(key);
                kept.push((path, snap));
            }
        }
        if hashes == cache.hashes {
            // Same paths and hashes: every note is a cache hit, nothing to copy.
            drop(cache);
            return Ok(parsed);
        }
        // Copy cache hits only when the listing changed.
        let notes = kept
            .into_iter()
            .filter_map(|(path, snap)| match snap {
                Some(snap) => Some(*snap),
                None => cache.notes.get(&path).cloned(),
            })
            .collect();
        // Read snapshots built from the previous notes keep sharing them.
        *cache = Parsed {
            hashes,
            notes: Snapshot::new(notes, Default::default()),
        };
        drop(cache);
        Ok(parsed)
    }

    /// Run `f` on the read snapshot of this publication (built once per
    /// generation and index watermark) with `evidence` installed. Reads
    /// judge each hit from its fresh bytes; the snapshot supplies the other
    /// notes (superseders, conflict partners), which never include the hit
    /// itself, so a cached copy of the hit cannot affect its own judgment.
    pub(crate) fn with_read_snapshot<T>(
        &self,
        evidence: wi_lifecycle::Evidence,
        f: impl FnOnce(&Snapshot) -> T,
    ) -> Result<T> {
        let key = (self.generation()?, self.index_as_of().ok().flatten());
        let current = self
            .lifecycle_cache
            .borrow()
            .built
            .as_ref()
            .is_some_and(|(k, _)| *k == key);
        if !current {
            // Drop the old snapshot before listing, so this handle never
            // holds it next to the new one.
            self.lifecycle_cache.borrow_mut().built = None;
            let parsed = self.refresh_lifecycle_notes()?;
            let notes = lock(&parsed).notes.clone();
            self.lifecycle_cache.borrow_mut().built = Some((key, notes));
        }
        let mut guard = self.lifecycle_cache.borrow_mut();
        let Some((_, snapshot)) = &mut guard.built else {
            return Err(crate::error::StoreError::Invalid("no read snapshot".into()));
        };
        snapshot.evidence = evidence;
        Ok(f(snapshot))
    }

    /// The vault snapshot with `fresh` notes replacing their listed versions
    /// (a note just written may not be indexed yet). Evidence is empty; add
    /// it with `evidence_for`.
    pub(crate) fn lifecycle_snapshot(&self, fresh: &[NoteSnapshot]) -> Result<Snapshot> {
        let parsed = self.refresh_lifecycle_notes()?;
        let parsed = lock(&parsed);
        let kept = |n: &&NoteSnapshot| {
            !fresh.iter().any(|f| f.path == n.path)
                && (n.lifecycle.lifecycle_type != wi_note::LifecycleType::Work
                    || self.owns_work_path(&n.path))
        };
        let all = parsed.notes.notes();
        if fresh.is_empty() && all.iter().all(|n| kept(&n)) {
            // Nothing replaced or left out: share the parsed notes. A pass
            // takes several snapshots of a large vault.
            return Ok(parsed.notes.clone());
        }
        // Clone only the kept notes, not the whole listing first.
        let notes = all
            .iter()
            .filter(kept)
            .cloned()
            .chain(fresh.iter().cloned())
            .collect();
        Ok(Snapshot::new(notes, Default::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::OpenOptions;

    fn rule(text: &str) -> String {
        format!(
            "---\ntype: memory\nkind: rule\nclaim_kind: intent\nstatus: active\nsummary: {text}\nentities: []\nscope: personal\napplicability: {{conditions: [], supersedes: []}}\nsources: []\norigin: {{kind: user, locator: file, captured_at: '2026-01-01T00:00:00Z'}}\n---\n{text}\n"
        )
    }

    fn summaries(vault: &Vault) -> Vec<String> {
        vault
            .current_rules()
            .unwrap()
            .rules
            .into_iter()
            .filter_map(|r| r.summary)
            .collect()
    }

    fn open(temp: &tempfile::TempDir, index: &str) -> Vault {
        Vault::open_with(
            temp.path().join("vault"),
            OpenOptions {
                index_path: Some(temp.path().join(index)),
            },
        )
        .unwrap()
    }

    #[test]
    fn handles_of_one_vault_and_index_share_parsed_notes_and_see_each_others_writes() {
        let temp = tempfile::tempdir().unwrap();
        let (writer, _) = Vault::init_with(
            temp.path().join("vault"),
            OpenOptions {
                index_path: Some(temp.path().join("index.db")),
            },
        )
        .unwrap();
        let path = writer.root().join("rule.md");
        std::fs::write(&path, rule("Use short paragraphs.")).unwrap();
        writer.catch_up().unwrap();
        let reader = open(&temp, "index.db");
        let other_index = open(&temp, "other.db");
        let shared = |a: &Vault, b: &Vault| {
            Arc::ptr_eq(
                &a.lifecycle_cache.borrow().parsed,
                &b.lifecycle_cache.borrow().parsed,
            )
        };
        assert!(shared(&writer, &reader));
        assert!(!shared(&writer, &other_index));
        assert_eq!(summaries(&reader), ["Use short paragraphs."]);
        // A change indexed through one handle reaches the other handle's
        // next listing; the shared notes never answer for an old hash.
        std::fs::write(&path, rule("Use verified records.")).unwrap();
        writer.catch_up().unwrap();
        assert_eq!(summaries(&reader), ["Use verified records."]);
        assert_eq!(summaries(&writer), ["Use verified records."]);
        // A private files-only cache leaves the shared one alone.
        reader.set_lifecycle_files_only(true);
        assert!(!shared(&writer, &reader));
        assert_eq!(summaries(&reader), ["Use verified records."]);
    }

    #[test]
    fn a_read_snapshot_keeps_its_view_while_another_handle_lists_new_notes() {
        let temp = tempfile::tempdir().unwrap();
        let (writer, _) = Vault::init_with(
            temp.path().join("vault"),
            OpenOptions {
                index_path: Some(temp.path().join("index.db")),
            },
        )
        .unwrap();
        let path = writer.root().join("rule.md");
        std::fs::write(&path, rule("First.")).unwrap();
        writer.catch_up().unwrap();
        let reader = open(&temp, "index.db");
        let summary = |s: &Snapshot| s.get("rule.md").and_then(|n| n.lifecycle.summary.clone());
        let seen = |vault: &Vault| {
            vault
                .with_read_snapshot(Default::default(), |s| summary(s))
                .unwrap()
        };
        assert_eq!(seen(&reader).as_deref(), Some("First."));
        // Another handle indexes a change and lists the new notes.
        std::fs::write(&path, rule("Second.")).unwrap();
        writer.catch_up().unwrap();
        let listed = writer.lifecycle_snapshot(&[]).unwrap();
        assert_eq!(summary(&listed).as_deref(), Some("Second."));
        // The reader's snapshot still holds the notes it was built from.
        let kept = reader.lifecycle_cache.borrow().built.clone().unwrap().1;
        assert_eq!(summary(&kept).as_deref(), Some("First."));
        assert!(!std::ptr::eq(kept.notes(), listed.notes()));
        // Its next read is a new publication and shares the new notes.
        assert_eq!(seen(&reader).as_deref(), Some("Second."));
        let rebuilt = reader.lifecycle_cache.borrow().built.clone().unwrap().1;
        assert!(std::ptr::eq(rebuilt.notes(), listed.notes()));
    }

    #[test]
    fn a_snapshot_with_nothing_left_out_shares_the_parsed_notes() {
        let temp = tempfile::tempdir().unwrap();
        let (vault, _) = Vault::init_with(
            temp.path().join("vault"),
            OpenOptions {
                index_path: Some(temp.path().join("index.db")),
            },
        )
        .unwrap();
        std::fs::write(vault.root().join("rule.md"), rule("Shared.")).unwrap();
        vault.catch_up().unwrap();
        let one = vault.lifecycle_snapshot(&[]).unwrap();
        let two = vault.lifecycle_snapshot(&[]).unwrap();
        assert!(std::ptr::eq(one.notes(), two.notes()));
        // The shared notes equal a copy built note by note.
        let copied: Vec<NoteSnapshot> = vault.lifecycle_notes_where(|_| true).unwrap();
        assert_eq!(
            serde_json::to_string(one.notes()).unwrap(),
            serde_json::to_string(&copied).unwrap()
        );
        // A replaced note gives a separate snapshot.
        let fresh = NoteSnapshot::from_bytes("rule.md", rule("Fresh.").as_bytes());
        let three = vault.lifecycle_snapshot(&[fresh]).unwrap();
        assert!(!std::ptr::eq(one.notes(), three.notes()));
        assert_eq!(
            three
                .get("rule.md")
                .and_then(|n| n.lifecycle.summary.clone())
                .as_deref(),
            Some("Fresh.")
        );
    }
}
