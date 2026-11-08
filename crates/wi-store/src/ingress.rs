//! Which working-tree files history has not seen, and the ingress-stat
//! cache that lets a one-shot command answer that without hashing.
//!
//! The cache is a disposable per-device file next to the index database
//! (`ingress-<vault_id>.stat`), never inside the vault. A stamp maps a
//! vault-relative path to the file's stat (size, mtime, ctime, inode) and
//! the git blob id of its bytes. A stamp is written only after those bytes
//! were correlated with the manifest or the branch tip, or after they were
//! ingested and then correlated; indexing alone never writes one, so a
//! search or reindex cannot hide a change from history.
//!
//! The cache records the branch tip it was verified against. When the tip
//! moved, every stat-unchanged stamp is compared with the new tree (no
//! hashing), and every path of the new tree that is gone from the working
//! tree is checked, so a deleted file is found even when it was never
//! stamped. A missing, unreadable or foreign cache means a full compare.
//! Paths the engine publishes are stamped when the vault flushes, by
//! appending stamp or removal lines to the cache file (later lines win), so
//! a flush costs the published paths and not the vault size. A full rewrite
//! compacts the file.
//! Limit: a change that keeps size, mtime, ctime and inode is found only
//! by a full compare (`wi commit`, `wi watch` start-up, `wi doctor`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use wi_history::{BlobId, History, Manifest, ManifestSnapshot, PublisherLock, blob_id_of};

use crate::contained::read_rel;
use crate::error::{Result, StoreError};
use crate::ingest::{
    IngestFailure, IngestOutcome, Known, ReconcileReport, known_in_manifest, walk_files,
    walk_files_parallel,
};
use crate::paths::is_engine_internal;
use crate::vault::Vault;
use crate::work::{WorkKind, WorkRequest};

const CACHE_MAGIC: &str = "wi-ingress-stat 1";
/// Up to this many paths are looked up one by one in the branch tip
/// instead of loading its whole tree (unmeasured policy choice).
const POINT_LOOKUPS: usize = 64;
/// Prefix of an appended line that removes a path's stamp.
const REMOVED: &str = "-\t";
/// Appended lines one handle writes before it rewrites the cache whole
/// (unmeasured policy choice; a repair or auto-ingest rewrite also
/// compacts).
const COMPACT_AFTER: usize = 2048;

/// How much of the vault to compare with history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Hash every ingress-eligible file (reconcile, doctor).
    All,
    /// Hash only files whose stat differs from their stamp (auto-ingest).
    /// Falls back to `All` when the cache is missing or invalid.
    StatChanged,
}

/// File stat compared before trusting a stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatKey {
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    ino: u64,
}

impl StatKey {
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        meta.file_type().is_file().then(|| Self::from_meta(&meta))
    }

    #[cfg(unix)]
    fn from_meta(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
            ino: meta.ino(),
        }
    }

    #[cfg(not(unix))]
    fn from_meta(meta: &std::fs::Metadata) -> Self {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or((0, 0), |d| {
                (d.as_secs() as i64, i64::from(d.subsec_nanos()))
            });
        Self {
            size: meta.len(),
            mtime,
            ctime: (0, 0),
            ino: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    stat: StatKey,
    blob: BlobId,
}

/// Parsed cache file.
#[derive(Debug, Default)]
struct StatCache {
    head: Option<String>,
    stamps: BTreeMap<String, Stamp>,
}

impl StatCache {
    /// Parse a cache file. Later lines override earlier ones, and a
    /// [`REMOVED`] line drops the path's stamp. A file that does not end
    /// with a line break (an interrupted append) is rejected whole.
    fn parse(text: &str, root: &str) -> Option<Self> {
        if !text.ends_with('\n') {
            return None;
        }
        let mut lines = text.lines();
        if lines.next()? != CACHE_MAGIC {
            return None;
        }
        if lines.next()?.strip_prefix("root\t")? != root {
            return None;
        }
        let head = match lines.next()?.strip_prefix("head\t")? {
            "-" => None,
            id => Some(id.to_owned()),
        };
        let mut stamps = BTreeMap::new();
        for line in lines {
            if let Some(path) = line.strip_prefix(REMOVED) {
                stamps.remove(path);
                continue;
            }
            let mut f = line.splitn(8, '\t');
            let mut num = || f.next()?.parse::<i64>().ok();
            let size = u64::try_from(num()?).ok()?;
            let mtime = (num()?, num()?);
            let ctime = (num()?, num()?);
            let ino = u64::try_from(num()?).ok()?;
            let blob = f.next()?;
            let path = f.next()?;
            if blob.len() != 40 || path.is_empty() {
                return None;
            }
            let stat = StatKey {
                size,
                mtime,
                ctime,
                ino,
            };
            let blob = BlobId(blob.to_owned());
            stamps.insert(path.to_owned(), Stamp { stat, blob });
        }
        Some(Self { head, stamps })
    }

    fn render(&self, root: &str) -> String {
        let mut out = format!(
            "{CACHE_MAGIC}\nroot\t{root}\nhead\t{}\n",
            self.head.as_deref().unwrap_or("-")
        );
        for (path, stamp) in &self.stamps {
            out.push_str(&stamp_line(path, stamp));
        }
        out
    }
}

fn stamp_line(path: &str, stamp: &Stamp) -> String {
    let s = stamp.stat;
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{path}\n",
        s.size, s.mtime.0, s.mtime.1, s.ctime.0, s.ctime.1, s.ino, stamp.blob
    )
}

/// A path the cache format can hold (one line, tab separated).
fn stampable(rel: &str) -> bool {
    !rel.contains(['\t', '\n', '\r'])
}

/// The ingress filter of `Vault::ingest_path` for paths the walk did not
/// visit (gone or cached paths).
struct IngressFilter<'a>(&'a Path);

impl IngressFilter<'_> {
    fn new(root: &Path) -> IngressFilter<'_> {
        IngressFilter(root)
    }

    fn excludes(&self, rel: &str) -> bool {
        is_engine_internal(rel) || wi_index::ignored(self.0, Path::new(rel))
    }
}

/// Result of comparing the working tree with history. Read-only.
#[derive(Debug, Default)]
pub(crate) struct Survey {
    /// Files walked (eligible or not).
    pub scanned: usize,
    /// Files whose bytes were read and hashed.
    pub hashed: usize,
    /// Eligible files whose bytes history does not know.
    pub unseen: Vec<String>,
    /// Stamped or committed paths no longer present (stat-scoped survey
    /// only).
    pub gone: Vec<String>,
    /// Eligible files that could not be read, with the error.
    pub unreadable: Vec<(String, String)>,
    /// The loaded cache already holds exactly these stamps for this tip,
    /// so storing it again would change nothing.
    cache_current: bool,
    stamps: BTreeMap<String, Stamp>,
    head: Option<String>,
}

/// History lookups for one survey: manifest first, then the branch tip
/// tree, loaded once on first use.
struct Lookup<'a> {
    vault: &'a Vault,
    snap: ManifestSnapshot,
    tree: Option<BTreeMap<String, BlobId>>,
    /// Branch tip for single-path lookups instead of loading the whole
    /// tree (`Some(None)`: no commit yet).
    point: Option<Option<String>>,
}

impl Lookup<'_> {
    fn known(&mut self, rel: &str) -> Result<Known> {
        if let Some(known) = known_in_manifest(&self.snap, rel) {
            return Ok(known);
        }
        if let Some(tip) = &self.point {
            let Some(tip) = tip else {
                return Ok(Known::Unknown);
            };
            return Ok(History::blob_at(&self.vault.history, tip, rel)?
                .map_or(Known::Unknown, Known::Blob));
        }
        if self.tree.is_none() {
            let tree = History::tree(&self.vault.history)?;
            self.tree = Some(tree.into_iter().collect());
        }
        Ok(self
            .tree
            .as_ref()
            .and_then(|t| t.get(rel).cloned())
            .map_or(Known::Unknown, Known::Blob))
    }

    /// Every path of the branch tip tree.
    fn tree_paths(&mut self) -> Result<Vec<String>> {
        if self.tree.is_none() {
            let tree = History::tree(&self.vault.history)?;
            self.tree = Some(tree.into_iter().collect());
        }
        Ok(self
            .tree
            .as_ref()
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default())
    }
}

impl Vault {
    fn ingress_cache_path(&self) -> Option<PathBuf> {
        let index = self.index_database().ok()??;
        Some(
            index
                .parent()?
                .join(format!("ingress-{}.stat", self.vault_id)),
        )
    }

    fn root_key(&self) -> String {
        self.root().display().to_string()
    }

    fn load_ingress_cache(&self) -> Option<StatCache> {
        let text = std::fs::read_to_string(self.ingress_cache_path()?).ok()?;
        StatCache::parse(&text, &self.root_key())
    }

    /// Replace the cache file. It is disposable: a failed write only costs
    /// a full compare next time, so the error is returned for reporting.
    fn store_ingress_cache(&self, cache: &StatCache) -> std::io::Result<()> {
        let Some(path) = self.ingress_cache_path() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!(
            "stat.tmp-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&tmp, cache.render(&self.root_key()))?;
        std::fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    fn lookup(&self) -> Result<Lookup<'_>> {
        let snap = {
            let guard = PublisherLock::acquire(&self.history)?;
            Manifest::snapshot(&self.history, &guard)?
        };
        Ok(Lookup {
            vault: self,
            snap,
            tree: None,
            point: None,
        })
    }

    /// Compare working-tree files with history without writing anything.
    pub(crate) fn survey(&self, scope: Scope) -> Result<Survey> {
        self.survey_with(scope, true)
    }

    /// [`Vault::survey`]; `parallel` stats files on every core, otherwise
    /// on the calling thread only (a background scan next to foreground
    /// requests).
    fn survey_with(&self, scope: Scope, parallel: bool) -> Result<Survey> {
        let cache = match scope {
            Scope::StatChanged => self.load_ingress_cache(),
            Scope::All => None,
        };
        let head = History::head(&self.history)?;
        let mut lookup = self.lookup()?;
        let tip_moved = cache.as_ref().is_some_and(|c| c.head != head);
        let filter = IngressFilter::new(self.root());
        let mut survey = Survey {
            head,
            ..Survey::default()
        };
        let root = self.root();
        // The walk already applies the vault's ignore rules and skips
        // engine-internal paths, the same rule as `IngressFilter`.
        let stats: Vec<(String, StatKey)> = if parallel {
            // Stat on the walker threads, as each directory is read.
            let walked = walk_files_parallel(root, |path| Some(StatKey::of(path)));
            survey.scanned = walked.len();
            walked
                .into_iter()
                .filter_map(|(rel, stat)| Some((rel, stat?)))
                .collect()
        } else {
            let walked = walk_files(root);
            survey.scanned = walked.len();
            walked
                .into_iter()
                .filter_map(|rel| StatKey::of(&root.join(&rel)).map(|stat| (rel, stat)))
                .collect()
        };
        // Sorted by path, so membership is a binary search.
        let present: Vec<String> = stats.iter().map(|(rel, _)| rel.clone()).collect();
        let is_present = |p: &str| present.binary_search_by(|q| q.as_str().cmp(p)).is_ok();
        for (rel, stat) in stats {
            let stamp = cache
                .as_ref()
                .and_then(|c| c.stamps.get(&rel))
                .filter(|s| s.stat == stat);
            if let Some(stamp) = stamp
                && stamp_still_known(&mut lookup, &rel, stamp, tip_moved)?
            {
                survey.stamps.insert(rel, stamp.clone());
                continue;
            }
            self.compare_file(&mut lookup, rel, stat, &mut survey)?;
        }
        let mut gone = BTreeSet::new();
        if let Some(cache) = &cache {
            gone.extend(
                cache
                    .stamps
                    .keys()
                    .filter(|p| !is_present(p) && !filter.excludes(p))
                    .cloned(),
            );
        }
        // A tip this cache did not verify (or no cache) may hold committed
        // paths that were never stamped; check the ones that are gone.
        if scope == Scope::StatChanged && (cache.is_none() || tip_moved) && survey.head.is_some() {
            gone.extend(
                lookup
                    .tree_paths()?
                    .into_iter()
                    .filter(|p| !is_present(p) && !filter.excludes(p)),
            );
        }
        survey.gone = gone.into_iter().collect();
        survey.cache_current = cache
            .is_some_and(|c| !tip_moved && survey.gone.is_empty() && c.stamps == survey.stamps);
        Ok(survey)
    }

    /// Hash one file and compare it with history.
    fn compare_file(
        &self,
        lookup: &mut Lookup<'_>,
        rel: String,
        before: StatKey,
        survey: &mut Survey,
    ) -> Result<()> {
        if self.sync_exclude_oversize(&rel)? {
            return Ok(());
        }
        let bytes = match read_rel(self.root(), &rel) {
            Ok(Some(bytes)) => bytes,
            // Gone since the walk, or no longer a regular file.
            Ok(None) | Err(StoreError::Invalid(_) | StoreError::Protected { .. }) => {
                return Ok(());
            }
            Err(e) => {
                survey.unreadable.push((rel, short_cause(&e)));
                return Ok(());
            }
        };
        survey.hashed += 1;
        let blob = blob_id_of(&bytes)?;
        if lookup.known(&rel)? != Known::Blob(blob.clone()) {
            survey.unseen.push(rel);
            return Ok(());
        }
        let after = StatKey::of(&self.root().join(&rel));
        if after == Some(before) && stampable(&rel) {
            survey.stamps.insert(rel, Stamp { stat: before, blob });
        }
        Ok(())
    }

    /// Stamps for `paths` whose current bytes history now knows (after
    /// they were ingested).
    fn stamps_after_ingest(&self, paths: &[String]) -> Result<BTreeMap<String, Stamp>> {
        let mut lookup = self.lookup()?;
        if paths.len() <= POINT_LOOKUPS {
            // A few paths: look each one up instead of loading the tree.
            lookup.point = Some(History::head(&self.history)?);
        }
        let mut survey = Survey::default();
        for rel in paths {
            if let Some(stat) = StatKey::of(&self.root().join(rel)) {
                self.compare_file(&mut lookup, rel.clone(), stat, &mut survey)?;
            }
        }
        Ok(survey.stamps)
    }

    /// Ingest the unseen and gone paths of `survey` exactly like
    /// `reconcile`, then store the stamps of every correlated file. A path
    /// that fails does not stop the others: it is reported in `failed`,
    /// gets an `ingest-failed` work request and stays unstamped, so the
    /// next run tries it again.
    pub(crate) fn ingest_survey(&self, survey: Survey, report: &mut ReconcileReport) -> Result<()> {
        report.scanned = survey.scanned;
        report.hashed = survey.hashed;
        for (rel, error) in &survey.unreadable {
            self.ingest_failed(rel, error, report)?;
        }
        let mut touched = Vec::new();
        for rel in survey.unseen.iter().chain(&survey.gone) {
            let ingested = match self.ingest_path(rel) {
                Ok(ingested) => ingested,
                Err(e) => {
                    self.ingest_failed(rel, &short_cause(&e), report)?;
                    continue;
                }
            };
            match ingested.outcome {
                IngestOutcome::Ingested { .. } => {
                    touched.push(rel.clone());
                    report.ingested.push(ingested);
                }
                IngestOutcome::Missing => report.missing.push(rel.clone()),
                IngestOutcome::Unchanged => touched.push(rel.clone()),
                IngestOutcome::Skipped { .. } => {}
            }
        }
        if !report.missing.is_empty() {
            self.commit_pending_deletions()?;
        }
        if survey.cache_current && touched.is_empty() {
            return Ok(());
        }
        let mut stamps = survey.stamps;
        stamps.extend(self.stamps_after_ingest(&touched)?);
        let cache = StatCache {
            head: survey.head,
            stamps,
        };
        // Disposable cache: a failed write only means a full compare next
        // time, never a missed change.
        let _ = self.store_ingress_cache(&cache);
        Ok(())
    }

    /// Record the work request for `rel`, which could not be ingested for
    /// the short plain `error`, and reopen its work note if a maintenance
    /// pass had closed it.
    fn ingest_failed(&self, rel: &str, error: &str, report: &mut ReconcileReport) -> Result<()> {
        let request = WorkRequest::new(
            WorkKind::IngestFailed,
            rel,
            format!("not in history yet: {error}"),
        );
        self.record_work(request.clone())?;
        self.reopen_resolved_request(&request)?;
        report.failed.push(IngestFailure {
            path: rel.to_owned(),
            error: error.to_owned(),
        });
        Ok(())
    }

    /// Stamp the paths the engine published since the last call, so a
    /// later deletion of one of them is found by the stat-scoped scan.
    /// Without a usable cache the next scan compares everything anyway.
    pub(crate) fn stamp_published(&self) -> Result<()> {
        let paths: Vec<String> = std::mem::take(&mut *self.published.borrow_mut())
            .into_iter()
            .collect();
        if paths.is_empty() {
            return Ok(());
        }
        let Some(path) = self.ingress_cache_path() else {
            return Ok(());
        };
        if !path.is_file() {
            return Ok(());
        }
        let stamps = self.stamps_after_ingest(&paths)?;
        let appended = self.stamps_appended.get() + paths.len();
        if appended >= COMPACT_AFTER {
            self.stamps_appended.set(0);
            if let Some(mut cache) = self.load_ingress_cache() {
                for rel in &paths {
                    cache.stamps.remove(rel);
                }
                cache.stamps.extend(stamps);
                // Disposable cache, as in `ingest_survey`.
                let _ = self.store_ingress_cache(&cache);
            }
            return Ok(());
        }
        self.stamps_appended.set(appended);
        let mut lines = String::new();
        for rel in paths.iter().filter(|rel| stampable(rel)) {
            match stamps.get(rel) {
                Some(stamp) => lines.push_str(&stamp_line(rel, stamp)),
                None => lines.push_str(&format!("{REMOVED}{rel}\n")),
            }
        }
        // Disposable cache: a lost append only costs a hash next time, and
        // an append that races a rewrite lands in the replaced file.
        let _ = append_existing(&path, lines.as_bytes());
        Ok(())
    }

    /// One-shot file ingress for any command: ingest (actor `watcher`, user
    /// origin) every file whose stat changed since it was last correlated
    /// with history and whose bytes history does not know. Unchanged files
    /// are not hashed; a missing cache means a full compare. Idempotent and
    /// safe next to a running watcher or MCP server (same publisher lock).
    /// A path that fails is reported in `failed` and the scan goes on.
    /// While a live watcher holds the lease ([`crate::lease`]) this does
    /// nothing, because the watcher ingests and commits. The caller
    /// flushes.
    pub fn auto_ingest(&self) -> Result<ReconcileReport> {
        if crate::lease::held(&self.history) {
            return Ok(ReconcileReport::default());
        }
        self.stat_scan(true)
    }

    /// The stat-scoped survey and ingest of [`Vault::auto_ingest`] without
    /// the lease check: the watcher's repair scan. It stats on the
    /// watcher thread only, so it does not take every core from the
    /// requests a long-lived process serves meanwhile. The caller flushes.
    pub(crate) fn repair_scan(&self) -> Result<ReconcileReport> {
        self.stat_scan(false)
    }

    fn stat_scan(&self, parallel: bool) -> Result<ReconcileReport> {
        let survey = self.survey_with(Scope::StatChanged, parallel)?;
        let mut report = ReconcileReport::default();
        self.ingest_survey(survey, &mut report)?;
        Ok(report)
    }

    /// Eligible files whose bytes history does not know (full compare,
    /// read-only; `wi doctor`).
    pub fn unseen_paths(&self) -> Result<Vec<String>> {
        Ok(self.survey(Scope::All)?.unseen)
    }

    /// One full read-only compare: unseen and unreadable files.
    pub(crate) fn full_survey(&self) -> Result<Survey> {
        self.survey(Scope::All)
    }
}

/// Append to an existing file in one write; never create it.
fn append_existing(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)?
        .write_all(bytes)
}

/// Whether a stat-unchanged stamp still names bytes history knows. Without
/// a tip move only pending manifest records can disagree with it.
fn stamp_still_known(
    lookup: &mut Lookup<'_>,
    rel: &str,
    stamp: &Stamp,
    tip_moved: bool,
) -> Result<bool> {
    let expected = Known::Blob(stamp.blob.clone());
    if let Some(known) = known_in_manifest(&lookup.snap, rel) {
        return Ok(known == expected);
    }
    if !tip_moved {
        return Ok(true);
    }
    Ok(lookup.known(rel)? == expected)
}

/// A short plain cause for a file that could not be ingested: no absolute
/// path and no raw operating-system error text, since it becomes the
/// reason of a synced work item.
pub(crate) fn short_cause(error: &StoreError) -> String {
    use std::io::ErrorKind;
    match error {
        StoreError::Io { source, .. } => match source.kind() {
            ErrorKind::PermissionDenied => "permission denied",
            ErrorKind::NotFound => "the file disappeared while it was read",
            ErrorKind::InvalidData => "the file content cannot be read",
            ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut => {
                "the read was interrupted"
            }
            _ => "the file cannot be read",
        }
        .to_owned(),
        StoreError::Parse(_) => "the file is not valid UTF-8".to_owned(),
        StoreError::ConflictMarkers { .. } => "the file has git conflict markers".to_owned(),
        other => other.code().replace('-', " "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_round_trips_and_rejects_another_root() {
        let mut cache = StatCache {
            head: Some("a".repeat(40)),
            ..StatCache::default()
        };
        cache.stamps.insert(
            "notes/a b.md".into(),
            Stamp {
                stat: StatKey {
                    size: 3,
                    mtime: (10, 20),
                    ctime: (-1, 5),
                    ino: 7,
                },
                blob: BlobId("b".repeat(40)),
            },
        );
        let text = cache.render("/v");
        let parsed = StatCache::parse(&text, "/v").unwrap();
        assert_eq!(parsed.head, cache.head);
        assert_eq!(parsed.stamps, cache.stamps);
        assert!(StatCache::parse(&text, "/other").is_none());
        assert!(StatCache::parse("garbage", "/v").is_none());
        assert!(StatCache::parse(&text.replace("\n3\t10\t", "\nx\t10\t"), "/v").is_none());
    }

    #[test]
    fn appended_lines_override_and_remove_stamps() {
        let stamp = |size| Stamp {
            stat: StatKey {
                size,
                mtime: (1, 2),
                ctime: (3, 4),
                ino: 5,
            },
            blob: BlobId("c".repeat(40)),
        };
        let mut cache = StatCache::default();
        cache.stamps.insert("a.md".into(), stamp(1));
        cache.stamps.insert("b.md".into(), stamp(1));
        let mut text = cache.render("/v");
        text.push_str(&stamp_line("a.md", &stamp(9)));
        text.push_str(&format!("{REMOVED}b.md\n"));
        text.push_str(&format!("{REMOVED}never-stamped.md\n"));
        let parsed = StatCache::parse(&text, "/v").unwrap();
        assert_eq!(parsed.stamps.len(), 1);
        assert_eq!(parsed.stamps["a.md"], stamp(9));
        // An interrupted append (no final line break) discards the cache.
        assert!(StatCache::parse(text.trim_end_matches('\n'), "/v").is_none());
    }

    #[test]
    fn paths_with_line_breaks_are_never_stamped() {
        assert!(stampable("a/b.md"));
        assert!(!stampable("a\tb.md"));
        assert!(!stampable("a\nb.md"));
    }
}
