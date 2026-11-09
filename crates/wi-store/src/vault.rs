//! The vault service: opening, recovery, index access and flushing.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use wi_history::{
    FlushReport, Flusher, HistoryError, RecoveryReport, Vault as HistoryVault, WorkingTree,
    durable, hash_bytes,
};
use wi_index::Index;
use wi_note::{VAULT_CONFIG_PATH, VaultConfig};

use crate::contained::{Dir, read_rel};
use crate::error::{Result, StoreError};
use crate::paths::{CONFLICT_MARK, Target};
use crate::work::{self, WorkRequest};

/// Environment variable naming a cache directory that replaces the
/// platform cache for the index (`<dir>/<vault_id>/index.sqlite3`).
pub const CACHE_DIR_ENV: &str = "WI_CACHE_DIR";
/// Config key naming the device that runs lifecycle maintenance in folder mode.
pub const PRIMARY_DEVICE_KEY: &str = "primary_device";

/// Who sent a request. The actor goes to the manifest and the `Wi-Actor`
/// trailer; it is separate from a note's `origin` (AUTHORITY).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ingress {
    /// The `wi` command line, used as a tool by the owner.
    Cli,
    /// An MCP tool call (agent ingress).
    Mcp,
    /// A file change seen by the watcher or a scan.
    Watcher,
    /// Document or vault import.
    Import,
    /// The engine itself (lifecycle bookkeeping); may set engine-owned fields.
    Engine,
    /// Git remote synchronization; note provenance stays in its bytes.
    Sync,
}

impl Ingress {
    /// Actor string recorded in the manifest and commit trailer.
    pub fn actor(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Mcp => "mcp",
            Self::Watcher => "watcher",
            Self::Import => "import",
            Self::Engine => "engine",
            Self::Sync => "sync",
        }
    }

    /// `origin.kind` for a note created through this ingress.
    pub fn origin_kind(self) -> Option<&'static str> {
        match self {
            Self::Cli | Self::Watcher => Some("user"),
            Self::Mcp => Some("agent"),
            Self::Import => Some("file"),
            Self::Engine | Self::Sync => None,
        }
    }
}

/// Whether the index reflects an acknowledged write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexState {
    Ok,
    /// The index update failed; the next open repairs it.
    Pending,
}

/// Result of an acknowledged (or replayed) write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WriteResult {
    pub path: String,
    /// `sha256:` of the bytes now at `path`.
    pub checksum: String,
    /// Hash the precondition saw (`None` for a create).
    pub previous_checksum: Option<String>,
    /// Manifest sequence id of the operation.
    pub seq: u64,
    /// Vault generation after the write (equals `seq` for a fresh write).
    pub generation: u64,
    pub index: IndexState,
    /// True when an operation id returned an earlier result unchanged.
    pub replayed: bool,
    /// An editor save displaced by the swap, kept at this path.
    pub conflict_copy: Option<String>,
    pub moved_from: Option<String>,
    /// What the lifecycle did after the write (adoption outcome, claim-edit
    /// candidate, fork, loss charge). Absent for plain writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<serde_json::Value>,
}

/// A test hook receiving the absolute target path.
pub type PathHook = Rc<dyn Fn(&Path)>;
/// A test hook receiving the read attempt number.
pub type AttemptHook = Rc<dyn Fn(u32)>;

/// Test hooks. Unset in normal operation.
#[derive(Clone, Default)]
pub struct Hooks {
    /// Runs immediately before loading file bytes during each current-read
    /// attempt, with the absolute path (read-count and race tests).
    pub before_read: Option<PathHook>,
    /// Runs under the publisher lock after the checksum check and before
    /// the swap, with the absolute target path (race tests).
    pub before_swap: Option<PathHook>,
    /// Runs after the swap and before the `done` record.
    pub after_swap: Option<PathHook>,
    /// Runs immediately before the final displaced-file read, after the swap
    /// and initial displaced-byte check, with the absolute target path.
    pub before_displaced_recheck: Option<PathHook>,
    /// Runs inside a current read after the hits are verified and before
    /// the generation is checked, with the attempt number.
    pub during_read: Option<AttemptHook>,
    /// Runs in file ingress before the publisher lock is taken, with the
    /// absolute path (ingress race tests).
    pub before_ingest_lock: Option<PathHook>,
    /// Runs in index repair and rebuild after the index update and before
    /// the pending marker is settled, with the marker path (race tests).
    pub after_index_repair: Option<PathHook>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks").finish_non_exhaustive()
    }
}

/// Options for opening a vault.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Explicit index database path (outside the vault). Defaults to
    /// `$WI_CACHE_DIR/<vault_id>/index.sqlite3`, else the platform cache.
    pub index_path: Option<PathBuf>,
}

/// Where a vault's history and per-device state live (S1, S2).
#[derive(Debug, Clone, Default)]
pub struct Placement {
    /// Explicit per-device data directory (default `$WI_DATA_DIR`, else the
    /// platform data directory); holds folder-mode history and local state.
    pub data_root: Option<PathBuf>,
    /// Storage mode for `init` of a vault without history on this device.
    /// `None` chooses folder mode inside a known file-synced folder, else
    /// git mode. An existing vault keeps its mode.
    pub storage: Option<wi_history::StorageMode>,
}

/// What `open` did before accepting writes.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OpenReport {
    pub recovery: RecoveryReport,
    /// Work requests recorded while opening (conflicts, quarantine).
    pub work_requests: Vec<WorkRequest>,
    /// Paths re-indexed because an earlier write left `index: pending`.
    pub index_repaired: Vec<String>,
    /// Temp files of a crashed write that were removed or kept.
    pub temp_cleanup: Vec<String>,
    /// Why a pending index repair could not run. The pending marker stays;
    /// reads, writes (`index: pending`) and literal search keep working.
    pub index_degraded: Option<String>,
}

/// Working-tree hashes for replay through checked directories: a path with
/// a symlink or non-directory component never matches a record, so replay
/// leaves it in conflict (both versions kept) instead of reading through it.
struct ContainedTree<'a> {
    root: &'a Path,
    /// Target paths of open intents recovery holds open (see `recover`).
    hold: &'a std::collections::BTreeSet<String>,
}

impl WorkingTree for ContainedTree<'_> {
    fn current_hash(&self, path: &str) -> std::io::Result<Option<String>> {
        if self.hold.contains(path) {
            return Err(std::io::Error::other(
                "held open: a path of this write needs attention",
            ));
        }
        match read_rel(self.root, path) {
            Ok(bytes) => Ok(bytes.map(|b| hash_bytes(&b))),
            Err(StoreError::Protected { .. }) => Ok(Some(format!("unsafe-path:{path}"))),
            Err(StoreError::Invalid(_)) => Ok(Some(format!("non-file:{path}"))),
            Err(StoreError::Io { source, .. }) => Err(source),
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    }
}

/// An opened vault. Not `Sync`: share it through a mutex, or open one per
/// thread (the watcher opens its own).
pub struct Vault {
    pub(crate) history: HistoryVault,
    pub(crate) config: VaultConfig,
    pub(crate) vault_id: String,
    device_id_cache: RefCell<Option<String>>,
    pub(crate) options: OpenOptions,
    pub(crate) placement: Placement,
    pub(crate) index: RefCell<Option<Index>>,
    /// File identity (device, inode) of the database the open index
    /// connection uses. Another process that deletes and recreates the
    /// database leaves this connection on the unlinked file; a changed
    /// identity drops it so the next use opens the current file.
    index_identity: std::cell::Cell<Option<(u64, u64)>>,
    /// Counts index connections this handle dropped (replaced database or
    /// [`Vault::close_index`]); see [`Vault::index_epoch`].
    index_epoch: std::cell::Cell<u64>,
    pub(crate) hooks: Hooks,
    /// Paths file ingress found deleted outside the engine; the next flush
    /// commits them as owner deletions.
    pub(crate) pending_deletions: RefCell<std::collections::BTreeSet<String>>,
    pub(crate) import_batch_id: RefCell<Option<(String, crate::import::BatchKind)>>,
    open_report: OpenReport,
    /// Parsed lifecycle notes keyed by path, refreshed by content hash.
    pub(crate) lifecycle_cache: RefCell<crate::lifecycle::Cache>,
    /// Test clock for lifecycle decisions (`None`: `WI_NOW`, else the system clock).
    pub(crate) clock: std::cell::Cell<Option<wi_lifecycle::Timestamp>>,
    /// Paths this handle published, stamped in the ingress cache on flush.
    pub(crate) published: RefCell<std::collections::BTreeSet<String>>,
    /// Open intents this handle's last recovery held open (sequence ids).
    /// Writes to their paths fail with `recovery-held`; other writes go on.
    pub(crate) held: RefCell<std::collections::BTreeSet<u64>>,
    /// Stamp lines this handle appended to the ingress cache since it
    /// last rewrote it.
    pub(crate) stamps_appended: std::cell::Cell<usize>,
    /// Set while the engine writes a note at a path it chose under a
    /// managed `.wiki-island/` folder on behalf of another ingress (a
    /// capture note).
    pub(crate) managed_write: std::cell::Cell<bool>,
    /// The next write or move targets an import record (see
    /// [`Vault::write_import_record`]).
    pub(crate) import_record: std::cell::Cell<bool>,
    /// Notes read so far by a time-sliced maintenance pass on this handle.
    pub(crate) maintain_scan: RefCell<Option<crate::maintain::Scan>>,
    /// Generation the detector input of the last complete maintenance pass
    /// reflects (see [`Vault::maintain_basis`]).
    pub(crate) maintain_basis: std::cell::Cell<Option<u64>>,
    /// Archive directories this vault used before the configured one.
    pub(crate) former_archive_dirs: Vec<String>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("root", &self.root())
            .field("vault_id", &self.vault_id)
            .finish_non_exhaustive()
    }
}

const GITIGNORE_LINES: &[&str] = &[
    ".wiki-island/local/",
    ".wiki-island-init.tmp/",
    ".*.wi-tmp-*",
    ".*.wi-moved-*",
    ".DS_Store",
    ".obsidian/",
    ".trash/",
];

/// Result of `Vault::init`.
#[derive(Debug, Clone, Serialize)]
pub struct InitReport {
    pub root: PathBuf,
    pub vault_id: String,
    pub created_config: bool,
    pub gitignore_updated: bool,
    pub commits: usize,
    /// Why the first complete index walk failed. Search then uses the
    /// literal fallback until a later catch-up completes the walk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_degraded: Option<String>,
}

impl Vault {
    fn check_platform() -> Result<()> {
        if cfg!(any(unix, windows)) {
            Ok(())
        } else {
            Err(StoreError::UnsupportedPlatform)
        }
    }

    /// Create (or complete) a vault at `root`: git repository, config with a
    /// `vault_id`, `.gitignore` entries and an initial commit. Safe to run
    /// again on an existing vault.
    pub fn init(root: impl AsRef<Path>) -> Result<(Self, InitReport)> {
        Self::init_with(root, OpenOptions::default())
    }

    /// `init` with explicit open options.
    pub fn init_with(root: impl AsRef<Path>, options: OpenOptions) -> Result<(Self, InitReport)> {
        Self::init_placed(root, options, Placement::default())
    }

    /// `init` with explicit open options and placement.
    pub fn init_placed(
        root: impl AsRef<Path>,
        options: OpenOptions,
        placement: Placement,
    ) -> Result<(Self, InitReport)> {
        Self::check_platform()?;
        let root =
            std::path::absolute(root.as_ref()).map_err(|e| StoreError::io(root.as_ref(), e))?;
        let data_root = placement.data_root.as_deref();
        let folder = match wi_history::storage_mode(&root, data_root)? {
            Some(existing) => existing == wi_history::StorageMode::Folder,
            None => match placement.storage {
                Some(requested) => requested == wi_history::StorageMode::Folder,
                None => wi_history::cloud_service(&root).is_some(),
            },
        };
        if folder {
            // Folder mode keys its external history by `vault_id`, so the
            // config exists first (default durability: there was no config).
            Self::ensure_vault_id(&root, durable::Durability::default())?;
        }
        let history = if folder {
            HistoryVault::init_folder(&root, data_root)?
        } else {
            HistoryVault::init(&root)?
        };
        let root = history.root().to_path_buf();
        let mode = history.durability();
        let config_path = root.join(VAULT_CONFIG_PATH);
        let existing = std::fs::read_to_string(&config_path).ok();
        let config = match &existing {
            Some(text) => VaultConfig::parse(text)?,
            None => VaultConfig::parse(
                "# wiki-island vault config. See docs/FORMAT.md#vault-layout-and-config.\n",
            )?,
        };
        let created_config = config.vault_id().is_none();
        let mut ingest = Vec::new();
        if created_config {
            let id = uuid::Uuid::new_v4().to_string();
            let next = config.set("vault_id", toml_edit::Value::from(id))?;
            if let Some(dir) = config_path.parent() {
                durable::create_dir_durable(dir, mode)?;
            }
            durable::write_atomic(&config_path, next.to_text().as_bytes(), mode)?;
            ingest.push(VAULT_CONFIG_PATH.to_owned());
        }
        // Folder mode keeps the rules in `<gitdir>/info/exclude`, so the
        // notes folder holds no `.gitignore` written by the engine.
        let folder = history.storage_mode() == wi_history::StorageMode::Folder;
        let gitignore = if folder {
            history.exclude_file()
        } else {
            root.join(".gitignore")
        };
        if folder && let Some(dir) = gitignore.parent() {
            durable::create_dir_durable(dir, mode)?;
        }
        let current = std::fs::read_to_string(&gitignore).unwrap_or_default();
        let missing: Vec<&str> = GITIGNORE_LINES
            .iter()
            .copied()
            .filter(|l| !current.lines().any(|c| c.trim() == *l))
            .collect();
        let gitignore_updated = !missing.is_empty();
        if gitignore_updated {
            let mut text = current.clone();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            for line in &missing {
                text.push_str(line);
                text.push('\n');
            }
            durable::write_atomic(&gitignore, text.as_bytes(), mode)?;
            if !folder {
                ingest.push(".gitignore".to_owned());
            }
        }
        let vault = Self::open_placed(&root, options, placement)?;
        let engine_files: &[&str] = if folder {
            &[VAULT_CONFIG_PATH]
        } else {
            &[VAULT_CONFIG_PATH, ".gitignore"]
        };
        for rel in engine_files {
            vault.ingest_engine_file(rel)?;
        }
        vault.ensure_primary_device()?;
        let report = vault.flush()?;
        // A complete first walk sets `index_as_of`, so a fresh vault never
        // answers from the literal fallback. A failure stays retryable.
        let index_degraded = vault.catch_up().err().map(|e| e.to_string());
        let info = InitReport {
            root,
            vault_id: vault.vault_id.clone(),
            created_config,
            gitignore_updated,
            commits: report.commits.len(),
            index_degraded,
        };
        Ok((vault, info))
    }

    /// Open the vault at `root`. Replays the manifest before accepting
    /// writes (settles a crashed write, commits acknowledged records,
    /// records work requests) and repairs a pending index update.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(root, OpenOptions::default())
    }

    /// `open` with explicit options.
    pub fn open_with(root: impl AsRef<Path>, options: OpenOptions) -> Result<Self> {
        Self::open_placed(root, options, Placement::default())
    }

    /// `open` with explicit options and placement.
    pub fn open_placed(
        root: impl AsRef<Path>,
        options: OpenOptions,
        placement: Placement,
    ) -> Result<Self> {
        Self::check_platform()?;
        let history = HistoryVault::open_in(root.as_ref(), placement.data_root.as_deref())
            .map_err(|e| match e {
                HistoryError::NotARepository(path) => StoreError::NotInitialised(path),
                other => other.into(),
            })?;
        wi_index::set_exclude_file(history.root(), Some(&history.exclude_file()));
        let config_path = history.root().join(VAULT_CONFIG_PATH);
        let text = std::fs::read_to_string(&config_path)
            .map_err(|_| StoreError::NotInitialised(history.root().to_path_buf()))?;
        let config = VaultConfig::parse(&text)?;
        let vault_id = config
            .vault_id()
            .ok_or_else(|| StoreError::NotInitialised(history.root().to_path_buf()))?
            .to_owned();
        let mut vault = Self {
            history,
            config,
            vault_id,
            device_id_cache: RefCell::new(None),
            options,
            placement,
            index: RefCell::new(None),
            index_identity: std::cell::Cell::new(None),
            index_epoch: std::cell::Cell::new(0),
            hooks: Hooks::default(),
            pending_deletions: RefCell::default(),
            import_batch_id: RefCell::new(None),
            open_report: OpenReport::default(),
            lifecycle_cache: RefCell::new(crate::lifecycle::Cache::default()),
            clock: std::cell::Cell::new(None),
            published: RefCell::default(),
            held: RefCell::default(),
            stamps_appended: std::cell::Cell::new(0),
            managed_write: std::cell::Cell::new(false),
            import_record: std::cell::Cell::new(false),
            maintain_scan: RefCell::new(None),
            maintain_basis: std::cell::Cell::new(None),
            former_archive_dirs: Vec::new(),
        };
        vault.former_archive_dirs = vault.record_archive_dir();
        vault.open_report = vault.recover()?;
        vault.cleanup_sync_scratch()?;
        vault.migrate_device_work()?;
        Ok(vault)
    }

    /// Install test hooks.
    pub fn set_hooks(&mut self, hooks: Hooks) {
        self.hooks = hooks;
    }

    /// What the last open did.
    pub fn open_report(&self) -> &OpenReport {
        &self.open_report
    }

    pub fn root(&self) -> &Path {
        self.history.root()
    }

    pub fn vault_id(&self) -> &str {
        &self.vault_id
    }

    /// Where this vault keeps its history on this device.
    pub fn storage_mode(&self) -> wi_history::StorageMode {
        self.history.storage_mode()
    }

    /// Per-device state directory (S2): `.wiki-island/local` in git mode,
    /// `<data>/vaults/<vault_id>/local` in folder mode.
    pub fn local_dir(&self) -> PathBuf {
        self.history.local_dir()
    }

    /// This device's id (created on first use in the data directory).
    pub fn device_id(&self) -> Result<String> {
        if let Some(id) = self.device_id_cache.borrow().as_ref() {
            return Ok(id.clone());
        }
        let id = wi_history::device_id(self.placement.data_root.as_deref())?;
        *self.device_id_cache.borrow_mut() = Some(id.clone());
        Ok(id)
    }

    /// The `primary_device` recorded in the vault config, if any. Read
    /// anew, so a setup through another handle is visible.
    pub fn primary_device(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.root().join(VAULT_CONFIG_PATH)).ok()?;
        VaultConfig::parse(&text)
            .ok()?
            .get(PRIMARY_DEVICE_KEY)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    }

    /// Whether this device runs lifecycle-mutating maintenance (S4). A
    /// git-mode vault has one writer per clone, so every device does; in
    /// folder mode only the recorded primary device does (no record: this
    /// device, until a setup records one).
    pub fn is_primary_device(&self) -> bool {
        if self.storage_mode() == wi_history::StorageMode::Git {
            return true;
        }
        match (self.primary_device(), self.device_id()) {
            (None, _) => true,
            (Some(primary), Ok(me)) => primary == me,
            (Some(_), Err(_)) => false,
        }
    }

    /// Record this device as `primary_device` when the config names none.
    pub(crate) fn ensure_primary_device(&self) -> Result<()> {
        if self.storage_mode() == wi_history::StorageMode::Folder && self.primary_device().is_none()
        {
            self.set_primary_device()?;
        }
        Ok(())
    }

    /// Record this device as the vault's `primary_device` (`wi setup
    /// --primary`): the config is rewritten (unknown keys kept) and
    /// published like init's config write.
    pub fn set_primary_device(&self) -> Result<()> {
        let id = self.device_id()?;
        let path = self.root().join(VAULT_CONFIG_PATH);
        let text = std::fs::read_to_string(&path).map_err(|e| StoreError::io(&path, e))?;
        let next = VaultConfig::parse(&text)?
            .set(PRIMARY_DEVICE_KEY, toml_edit::Value::from(id))?
            .to_text();
        if next != text {
            durable::write_atomic(&path, next.as_bytes(), self.history.durability())?;
            self.ingest_engine_file(VAULT_CONFIG_PATH)?;
            self.flush()?;
        }
        Ok(())
    }

    /// Create `.wiki-island/config.toml` with a fresh `vault_id` when the
    /// config is missing or has none.
    fn ensure_vault_id(root: &Path, mode: durable::Durability) -> Result<()> {
        let config_path = root.join(VAULT_CONFIG_PATH);
        let existing = std::fs::read_to_string(&config_path).ok();
        let config = VaultConfig::parse(existing.as_deref().unwrap_or(
            "# wiki-island vault config. See docs/FORMAT.md#vault-layout-and-config.\n",
        ))?;
        if config.vault_id().is_some() {
            return Ok(());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let next = config.set("vault_id", toml_edit::Value::from(id))?;
        if let Some(dir) = config_path.parent() {
            durable::create_dir_durable(dir, mode)?;
        }
        durable::write_atomic(&config_path, next.to_text().as_bytes(), mode)?;
        Ok(())
    }

    pub fn config(&self) -> &VaultConfig {
        &self.config
    }

    /// The underlying history vault (git and manifest).
    pub fn history_vault(&self) -> &HistoryVault {
        &self.history
    }

    /// The vault generation: sequence id of the last published record.
    pub fn generation(&self) -> Result<u64> {
        Ok(wi_history::generation(&self.history)?)
    }

    /// The publication sequence now, for a dependency-scoped read check.
    pub fn publication_mark(&self) -> Result<wi_history::PublicationMark> {
        Ok(wi_history::publication_mark(&self.history)?)
    }

    /// Paths published, moved or opened after `mark`; `None` when unknown
    /// (every path must then count as changed).
    pub fn changed_since(
        &self,
        mark: &wi_history::PublicationMark,
    ) -> Result<Option<std::collections::BTreeSet<String>>> {
        Ok(wi_history::changed_paths_since(&self.history, mark)?)
    }

    /// Recorded work requests, oldest first.
    pub fn work_requests(&self) -> Result<Vec<WorkRequest>> {
        work::list(&self.history)
    }

    pub(crate) fn record_work(&self, request: WorkRequest) -> Result<()> {
        work::record(&self.history, &request)?;
        Ok(())
    }

    /// A note's git history, newest first (renames not followed).
    pub fn history(&self, path: &str) -> Result<Vec<wi_history::HistoryEntry>> {
        let rel = crate::paths::normalize(self.root(), path)?;
        Ok(wi_history::History::log(&self.history, &rel)?)
    }

    /// Histories in input path order, using one commit-graph traversal.
    pub fn history_many(
        &self,
        paths: &[&str],
        limit_per_path: Option<usize>,
    ) -> Result<Vec<(String, Vec<wi_history::HistoryEntry>)>> {
        let paths = paths
            .iter()
            .map(|p| crate::paths::normalize(self.root(), p))
            .collect::<Result<Vec<_>>>()?;
        let paths: Vec<_> = paths.iter().map(String::as_str).collect();
        Ok(wi_history::History::log_many(
            &self.history,
            &paths,
            limit_per_path,
        )?)
    }

    /// Commit pending manifest records (one commit per operation).
    pub fn flush(&self) -> Result<FlushReport> {
        self.recover_sync()?;
        if crate::fault::fail_flush_once() {
            return Err(StoreError::Invalid("injected flush failure".into()));
        }
        // Renames are judged before the flush retires the records.
        let deletions = self.plan_deletions()?;
        let flushed = Flusher::flush(&self.history);
        // Stamps record what the manifest knows, committed or not.
        self.stamp_published()?;
        let flushed = flushed?;
        self.record_deletions(&deletions)?;
        Ok(flushed)
    }
}

mod index;
mod recovery;

/// The vault-relative path of `name` in the directory of `rel`.
pub(crate) fn rel_in(rel: &str, name: &str) -> String {
    match rel.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.to_owned(),
    }
}

/// Existing conflict copies of `target` for `seq`: the deterministic name
/// `.<name>.wi-conflict-<seq>` and its `-<n>` variants.
pub(crate) fn conflict_copy_names(dir: &Dir, target: &Target, seq: u64) -> Result<Vec<String>> {
    let base = target.sibling_name(CONFLICT_MARK, &seq.to_string());
    let mut out: Vec<String> = dir
        .names()?
        .into_iter()
        .filter(|n| {
            n == &base
                || n.strip_prefix(&base)
                    .and_then(|rest| rest.strip_prefix('-'))
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    out.sort();
    Ok(out)
}

/// A conflict-copy name for `target` and `seq` that is not taken: the
/// deterministic name, else `-2`, `-3`, ... so a copy never replaces
/// another one.
pub(crate) fn free_conflict_name(dir: &Dir, target: &Target, seq: u64) -> Result<String> {
    let base = target.sibling_name(CONFLICT_MARK, &seq.to_string());
    if !dir.exists(&base)? {
        return Ok(base);
    }
    let mut n = 2u32;
    loop {
        let name = format!("{base}-{n}");
        if !dir.exists(&name)? {
            return Ok(name);
        }
        n += 1;
    }
}
