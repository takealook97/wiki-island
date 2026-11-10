//! Current reads with generation checks (OPERATIONS publisher and
//! manifest, PROTOCOL results).
//!
//! A read takes the publication state (generation and any open intent) before
//! and after reading the bytes. It is consistent when both are equal and no
//! intent is open: an open intent may already have swapped the file while
//! the generation still names the previous publication. Otherwise the read
//! retries once; if it is still not consistent, the result carries a
//! `generation-changed` diagnostic. Each returned hit (at most 8)
//! is re-hashed against its file; a changed hit is re-indexed and withheld
//! from this answer.

use serde::Serialize;
use wi_history::hash_bytes;
use wi_index::{Query, SearchHit};

use crate::contained::{read_rel, read_rel_bounded};
use crate::error::{Result, StoreError};
use crate::paths::resolve_readable;
use crate::vault::Vault;

/// A readable regular file's current metadata, without loading its body.
#[derive(Debug, Clone)]
pub struct NameStat {
    pub size: u64,
    pub mtime_ns: String,
    pub modified: Option<std::time::SystemTime>,
}

/// Hits re-verified per read (PROTOCOL: at most 8 hits).
pub const VERIFY_LIMIT: usize = 8;
/// Diagnostic code for a generation that changed twice during a read.
pub const GENERATION_CHANGED: &str = "generation-changed";

/// A note read.
#[derive(Debug, Clone, Serialize)]
pub struct ReadResult {
    pub path: String,
    #[serde(skip)]
    pub bytes: Vec<u8>,
    pub checksum: String,
    pub generation: u64,
    pub diagnostics: Vec<String>,
}

/// A withheld-hit count with its reason; no content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Withheld {
    pub reason: String,
    pub count: usize,
}

/// Search options.
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchOptions {
    /// Run a stat-walk catch-up first (one-shot CLI queries without a
    /// watcher).
    pub catch_up: bool,
}

/// A search answer.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub index_as_of: Option<String>,
    pub generation: u64,
    pub fallback: bool,
    pub withheld: Vec<Withheld>,
    pub diagnostics: Vec<String>,
    /// Index rows examined before hash verification removed changed hits;
    /// fewer than `Query::limit` means the match list is exhausted.
    pub examined: usize,
    /// Matching notes counted up to `Query::count_limit + 1`, when asked.
    pub matches: Option<usize>,
}

impl Vault {
    /// Generation and open-intent sequence id, read without a lock.
    pub(crate) fn publication_state(&self) -> Result<(u64, Option<u64>)> {
        Ok(wi_history::publication_state(&self.history)?)
    }

    /// Read a note or file with its checksum and generation, whatever its
    /// size. For write-path steps and internal callers that need the whole
    /// file; content delivered to a caller goes through [`Vault::read_bounded`].
    pub fn read(&self, path: &str) -> Result<ReadResult> {
        self.read_with_limit(path, None)
    }

    /// [`Vault::read`] refusing a file above the vault's `max_note_bytes`
    /// with [`StoreError::TooLarge`], without reading it whole.
    pub fn read_bounded(&self, path: &str) -> Result<ReadResult> {
        self.read_with_limit(path, Some(self.max_read_bytes()))
    }

    /// The byte limit for reads a caller aims at a file.
    pub(crate) fn max_read_bytes(&self) -> u64 {
        self.config.max_note_bytes()
    }

    fn read_with_limit(&self, path: &str, limit: Option<u64>) -> Result<ReadResult> {
        let target = resolve_readable(self.root(), path)?;
        let mut diagnostics = Vec::new();
        for attempt in 0..2 {
            let start = self.publication_state()?;
            if let Some(hook) = &self.hooks.before_read {
                hook(&target.abs);
            }
            let bytes = match limit {
                Some(max_bytes) => read_rel_bounded(self.root(), &target.rel, max_bytes)?,
                None => read_rel(self.root(), &target.rel)?,
            }
            .ok_or_else(|| StoreError::NotFound {
                path: target.rel.clone(),
            })?;
            let end = self.publication_state()?;
            let stable = start == end && end.1.is_none();
            if stable || attempt == 1 {
                if !stable {
                    diagnostics.push(GENERATION_CHANGED.to_owned());
                }
                return Ok(ReadResult {
                    path: target.rel,
                    checksum: hash_bytes(&bytes),
                    bytes,
                    generation: end.0,
                    diagnostics,
                });
            }
        }
        unreachable!("the loop returns on its second attempt")
    }

    /// Partial indexed name projection, available only after a complete walk.
    /// Merge with filesystem discovery and use only rows whose stamps match.
    pub fn name_rows(&self) -> Result<Option<Vec<wi_index::NameRow>>> {
        self.with_index(|index| Ok(index.name_rows()?))
    }

    /// Check containment, readability and regular-file kind without reading
    /// bytes. The descriptor is opened with the same flags as a full read.
    #[cfg(unix)]
    pub fn name_stat(&self, path: &str) -> Result<NameStat> {
        use rustix::fs::{Mode, OFlags};
        use std::time::UNIX_EPOCH;
        let target = resolve_readable(self.root(), path)?;
        let (dir, name) = crate::contained::Dir::open_parent(self.root(), &target.rel)?
            .ok_or_else(|| StoreError::NotFound {
                path: target.rel.clone(),
            })?;
        let fd = rustix::fs::openat(
            dir.file(),
            name.as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| StoreError::io(&target.abs, std::io::Error::from(e)))?;
        let file = std::fs::File::from(fd);
        let metadata = file
            .metadata()
            .map_err(|e| StoreError::io(&target.abs, e))?;
        if !metadata.is_file() {
            return Err(StoreError::Invalid("not a regular file".into()));
        }
        let modified = metadata.modified().ok();
        let mtime_ns = modified
            .map(|t| {
                t.duration_since(UNIX_EPOCH).map_or_else(
                    |e| format!("-{}", e.duration().as_nanos()),
                    |d| d.as_nanos().to_string(),
                )
            })
            .unwrap_or_default();
        Ok(NameStat {
            size: metadata.len(),
            mtime_ns,
            modified,
        })
    }

    /// Check containment, readability and regular-file kind without reading
    /// bytes, through the same checked handles as a full read.
    #[cfg(windows)]
    pub fn name_stat(&self, path: &str) -> Result<NameStat> {
        let target = resolve_readable(self.root(), path)?;
        let (dir, name) = crate::contained::Dir::open_parent(self.root(), &target.rel)?
            .ok_or_else(|| StoreError::NotFound {
                path: target.rel.clone(),
            })?;
        let file = dir
            .open_regular(&name)?
            .ok_or_else(|| StoreError::NotFound {
                path: target.rel.clone(),
            })?;
        let stat = wi_sys::fs::stamp(&file).map_err(|e| StoreError::io(&target.abs, e))?;
        let modified = file.metadata().ok().and_then(|m| m.modified().ok());
        Ok(NameStat {
            size: stat.size,
            mtime_ns: crate::contained::filetime_ns(stat.last_write),
            modified,
        })
    }

    #[cfg(not(any(unix, windows)))]
    pub fn name_stat(&self, _path: &str) -> Result<NameStat> {
        Err(StoreError::UnsupportedPlatform)
    }

    /// Bounded linked-note graph from the index. Consumers must re-read and
    /// hash-check delivered nodes and check the generation, as MCP does.
    pub fn context(&self, permalink: &str, depth: usize) -> Result<wi_index::ContextResult> {
        self.with_index(|index| Ok(index.context(permalink, depth)?))
    }

    /// Search the device index (literal fallback when it is unusable),
    /// verify the returned hits and check the generation.
    pub fn search(&self, query: &Query, options: SearchOptions) -> Result<SearchResponse> {
        let mut diagnostics = Vec::new();
        if options.catch_up
            && let Err(e) = self.catch_up()
        {
            diagnostics.push(format!("catch-up failed: {e}"));
        }
        let mut last = None;
        for attempt in 0..2u32 {
            let start = self.publication_state()?;
            let response = self.search_once(query, &mut diagnostics)?;
            if let Some(hook) = &self.hooks.during_read {
                hook(attempt);
            }
            let end = self.publication_state()?;
            let stable = start == end && end.1.is_none();
            last = Some((response, end.0));
            if stable {
                break;
            }
            if attempt == 1 {
                diagnostics.push(GENERATION_CHANGED.to_owned());
            }
        }
        let Some((once, generation)) = last else {
            return Err(StoreError::Invalid("search produced no attempt".into()));
        };
        Ok(SearchResponse {
            hits: once.hits,
            index_as_of: once.index_as_of,
            generation,
            fallback: once.fallback,
            withheld: once.withheld,
            diagnostics,
            examined: once.examined,
            matches: once.matches,
        })
    }

    fn search_once(&self, query: &Query, diagnostics: &mut Vec<String>) -> Result<SearchOnce> {
        let result = match self.with_index(|index| Ok(index.search(query)?)) {
            Ok(result) => result,
            Err(StoreError::Index(
                e @ (wi_index::Error::InvalidQuery(_) | wi_index::Error::QueryTooLong { .. }),
            )) => {
                return Err(StoreError::Index(e));
            }
            Err(e) => {
                diagnostics.push(format!("index unavailable ({e}); literal scan used"));
                self.close_index();
                wi_index::literal_search(self.root(), query)?
            }
        };
        // Preserve per-file index/fallback diagnostics at the service boundary;
        // MCP later emits bounded code counts, never these raw paths/messages.
        diagnostics.extend(result.diagnostics.iter().map(|d| d.message.clone()));
        let skipped = result.skipped.len();
        let examined = result.hits.len();
        let mut changed = Vec::new();
        let mut hits = Vec::with_capacity(result.hits.len());
        for (i, hit) in result.hits.into_iter().enumerate() {
            if i >= VERIFY_LIMIT {
                hits.push(hit);
                continue;
            }
            let rel = crate::paths::rel_string(&hit.rel_path);
            let current = rel
                .as_deref()
                .and_then(|rel| {
                    read_rel_bounded(self.root(), rel, self.max_read_bytes())
                        .ok()
                        .flatten()
                })
                .map(|b| hash_bytes(&b));
            if current.as_deref() == Some(hit.content_hash.as_str()) {
                hits.push(hit);
            } else {
                changed.push(rel.unwrap_or_else(|| hit.rel_path.to_string_lossy().into_owned()));
            }
        }
        let mut withheld = Vec::new();
        if skipped > 0 {
            withheld.push(Withheld {
                reason: "unsupported-file".into(),
                count: skipped,
            });
        }
        if !changed.is_empty() {
            withheld.push(Withheld {
                reason: "changed-after-index".into(),
                count: changed.len(),
            });
            let paths: Vec<&str> = changed.iter().map(String::as_str).collect();
            let _ = self.index_update(&paths);
        }
        Ok(SearchOnce {
            hits,
            index_as_of: result.index_as_of,
            fallback: result.fallback,
            withheld,
            examined,
            matches: result.matches,
        })
    }
}

/// One verified search attempt.
struct SearchOnce {
    hits: Vec<SearchHit>,
    index_as_of: Option<String>,
    fallback: bool,
    withheld: Vec<Withheld>,
    examined: usize,
    matches: Option<usize>,
}
