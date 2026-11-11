//! Lifecycle notes as engine snapshots, listed from the device index and
//! parsed once per content hash.

use std::collections::{BTreeMap, HashMap};

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

/// Parsed lifecycle notes and resolved evidence times, per vault handle.
///
/// Each parsed note is held once: `notes` is both the parse cache (keyed by
/// path, checked against `hashes`) and, while `built` names it, the read
/// snapshot. Reusing the cached notes avoids holding a duplicate snapshot.
#[derive(Debug, Default)]
pub(crate) struct Cache {
    /// path → hash of the parsed bytes of the note `notes` holds for it
    hashes: BTreeMap<String, String>,
    /// The notes of the latest listing, sorted by path.
    notes: Snapshot,
    /// The publication the read snapshot was built for. `None` inside
    /// means `notes` is that snapshot; `Some` keeps the snapshot built
    /// before a later listing in the same publication saw different notes,
    /// so reads keep the view they started with.
    built: Option<(BuiltKey, Option<Snapshot>)>,
    /// (ref, source_hash, commit) → engine-recorded evidence time
    pub evidence_times: HashMap<(String, String, Option<String>), Option<Timestamp>>,
    /// Ignore materialized snapshots in the index (differential tests).
    pub files_only: bool,
}

impl Cache {
    /// The cached snapshot of `path` when it was parsed from `hash`.
    fn hit(&self, path: &str, hash: &str) -> Option<&NoteSnapshot> {
        (self.hashes.get(path).map(String::as_str) == Some(hash))
            .then(|| self.notes.get(path))
            .flatten()
    }

    /// Replace the cached notes with a new listing. When the listing
    /// differs and a read snapshot was built from the current notes, that
    /// snapshot is kept aside first.
    fn replace(&mut self, hashes: BTreeMap<String, String>, notes: Vec<NoteSnapshot>) {
        if hashes == self.hashes {
            // Same paths and hashes: the same parsed notes.
            return;
        }
        let previous = std::mem::replace(&mut self.notes, Snapshot::new(notes, Default::default()));
        if let Some((_, kept @ None)) = &mut self.built {
            *kept = Some(previous);
        }
        self.hashes = hashes;
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

/// Resolve one directory's listed notes: a cached snapshot of the indexed
/// hash; else the materialized snapshot when the file's stamp still equals
/// the indexed stamp (the stat-walk rule); else the file's current bytes.
/// The directory chain is opened once, without following symlinks.
fn resolve_group(
    root: &std::path::Path,
    group: Vec<Listed>,
    cached: &Cache,
) -> Vec<Option<(String, String, NoteSnapshot)>> {
    let mut dir: Option<Option<Dir>> = None;
    group
        .into_iter()
        .map(|entry| {
            if let Some(hash) = &entry.hash
                && let Some(snap) = cached.hit(&entry.path, hash)
            {
                return Some((entry.path, hash.clone(), snap.clone()));
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
                return Some((entry.path, hash.clone(), snap));
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
            Some((entry.path, key, snap))
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
        let cache = self.lifecycle_cache.borrow();
        if cache.files_only {
            return listing;
        }
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

    /// Every lifecycle note (memory, capture, work), sorted by path. Notes
    /// whose bytes changed since the index saw them are parsed from disk;
    /// unchanged ones come from the handle cache or the index.
    pub(crate) fn lifecycle_notes(&self) -> Result<Vec<NoteSnapshot>> {
        self.refresh_lifecycle_notes()?;
        Ok(self.lifecycle_cache.borrow().notes.notes().to_vec())
    }

    /// Bring the handle cache up to date with the current listing.
    fn refresh_lifecycle_notes(&self) -> Result<()> {
        let listing = self.with_materialized(self.lifecycle_listing());
        let mut cache = self.lifecycle_cache.borrow_mut();
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
        let resolved: Vec<Option<(String, String, NoteSnapshot)>> = groups
            .into_par_iter()
            .flat_map_iter(|group| resolve_group(root, group, cached))
            .collect();
        let mut hashes = BTreeMap::new();
        let mut notes = Vec::with_capacity(resolved.len());
        for (path, key, snap) in resolved.into_iter().flatten() {
            // The first entry of a path wins, as in `Snapshot::new`.
            if let std::collections::btree_map::Entry::Vacant(slot) = hashes.entry(path) {
                slot.insert(key);
                notes.push(snap);
            }
        }
        cache.replace(hashes, notes);
        Ok(())
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
            // Drop the old snapshot before listing, so it is never held
            // next to the new one.
            self.lifecycle_cache.borrow_mut().built = None;
            self.refresh_lifecycle_notes()?;
            self.lifecycle_cache.borrow_mut().built = Some((key, None));
        }
        let mut guard = self.lifecycle_cache.borrow_mut();
        let cache = &mut *guard;
        let snapshot = match &mut cache.built {
            Some((_, Some(kept))) => kept,
            Some((_, None)) => &mut cache.notes,
            None => return Err(crate::error::StoreError::Invalid("no read snapshot".into())),
        };
        snapshot.evidence = evidence;
        Ok(f(snapshot))
    }

    /// The vault snapshot with `fresh` notes replacing their listed versions
    /// (a note just written may not be indexed yet). Evidence is empty; add
    /// it with `evidence_for`.
    pub(crate) fn lifecycle_snapshot(&self, fresh: &[NoteSnapshot]) -> Result<Snapshot> {
        self.refresh_lifecycle_notes()?;
        // Clone only the kept notes from the handle cache, not the whole
        // listing first: a pass takes several snapshots of a large vault.
        let notes = self
            .lifecycle_cache
            .borrow()
            .notes
            .notes()
            .iter()
            .filter(|n| !fresh.iter().any(|f| f.path == n.path))
            .filter(|n| {
                n.lifecycle.lifecycle_type != wi_note::LifecycleType::Work
                    || self.owns_work_path(&n.path)
            })
            .cloned()
            .chain(fresh.iter().cloned())
            .collect();
        Ok(Snapshot::new(notes, Default::default()))
    }
}
