//! The file watcher (`wi watch`, and inside the MCP server). It is
//! not a daemon and nothing requires it.
//!
//! Events are debounced (`watch_debounce_ms`, default 200 ms, an
//! unmeasured policy choice), then each changed path goes through file
//! ingress ([`Vault::ingest_path`]): engine writes are recognized by path
//! and hash and ignored; other changes are published as `ingest` records,
//! indexed, and committed by a flush after the batch. A failed flush is
//! retried every [`FLUSH_RETRY`] even without new events, and once more
//! when the watcher stops; at stop, pending records are flushed either way. While it runs the watcher holds the watcher
//! lease ([`crate::lease`]), so one-shot commands skip their own ingest.
//! Because events can be missed (a file unreadable when its event came, a
//! directory event that is not expanded, a backend error), the watcher
//! also runs the stat-scoped repair scan of `auto_ingest`: shortly
//! after start, every [`REPAIR_INTERVAL`], and soon after a backend error
//! or a skipped directory event. A failed repair withdraws the lease until
//! a later repair succeeds, so one-shot commands ingest meanwhile. After
//! every repair, failed or not, pending manifest records are flushed.
//! The first scan waits [`REPAIR_START_DELAY`], because `wi watch` and
//! `wi mcp` reconcile (a full compare) right after starting the watcher;
//! an immediate scan would repeat that work while it competes with the
//! first requests.
//!
//! The watcher also keeps the vault maintained ([`Vault::maintain`]): one
//! pass after its first repair scan, then at most once per
//! [`MAINTAIN_INTERVAL`] and only when the vault generation changed since
//! the last pass, plus one full pass per [`FULL_MAINTAIN_INTERVAL`]. An
//! idle vault costs one generation read per interval. Each pass runs on
//! the watcher thread between events with a [`MAINTAIN_SLICE`] budget; a
//! pass the budget stopped continues on the next tick.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer};
use serde::Serialize;

use crate::error::{Result, StoreError};
use crate::ingest::{IngestOutcome, IngestReport};
use crate::maintain::{MaintainOptions, MaintainReport};
use crate::vault::{OpenOptions, Vault};

/// Config key for the debounce interval in milliseconds.
pub const DEBOUNCE_KEY: &str = "watch_debounce_ms";
const DEFAULT_DEBOUNCE_MS: u64 = 200;
/// Retry interval after a failed batch flush, also without new events
/// (unmeasured policy choice).
pub const FLUSH_RETRY: Duration = Duration::from_secs(5);
/// Interval of the watcher's stat-scoped repair scan (unmeasured
/// policy choice). It also runs [`REPAIR_START_DELAY`] after start, soon
/// after a backend error or a skipped directory event, and every
/// [`FLUSH_RETRY`] after a failure.
pub const REPAIR_INTERVAL: Duration = Duration::from_secs(30);
/// Delay of the first repair scan after the watcher starts (unmeasured
/// policy choice, capped by the repair interval).
pub const REPAIR_START_DELAY: Duration = Duration::from_secs(10);
/// How long [`Vault::watch`] waits for the watcher to register before it
/// fails and cancels the thread.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Least time between two maintenance passes; a pass runs only when the
/// vault generation changed since the last one (unmeasured policy choice).
pub const MAINTAIN_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Interval of the full maintenance pass (unmeasured policy choice).
pub const FULL_MAINTAIN_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Time budget of one maintenance slice on the watcher thread.
pub const MAINTAIN_SLICE: Duration = Duration::from_secs(2);
/// Triggers within this window share one repair scan.
const REPAIR_COALESCE: Duration = Duration::from_secs(1);

/// Something the watcher did.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum WatchEvent {
    Ready,
    Ingest(Box<IngestReport>),
    Flushed { commits: usize },
    Error { message: String },
}

/// A running watcher. Dropping it stops the watcher thread.
pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    events: Receiver<WatchEvent>,
    backend: Sender<DebounceEventResult>,
}

impl WatchHandle {
    /// Events in the order they happened.
    pub fn events(&self) -> &Receiver<WatchEvent> {
        &self.events
    }

    /// Deliver a synthetic backend error to the watcher loop (tests).
    #[doc(hidden)]
    pub fn inject_backend_error(&self, message: &str) {
        let _ = self
            .backend
            .send(Err(notify_debouncer_mini::notify::Error::generic(message)));
    }

    /// Stop and join the watcher thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Vault {
    /// Debounce interval from config.
    pub fn debounce(&self) -> Duration {
        let ms = self
            .config
            .get(DEBOUNCE_KEY)
            .and_then(|i| i.as_integer())
            .and_then(|n| u64::try_from(n).ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_DEBOUNCE_MS);
        Duration::from_millis(ms)
    }

    /// Start a watcher thread with its own vault handle. Returns once the
    /// file system watch is registered.
    pub fn watch(&self) -> Result<WatchHandle> {
        self.watch_tuned(WatchTuning::default())
    }

    /// [`Vault::watch`] with test-only timing and fault knobs. Not a user
    /// setting.
    #[doc(hidden)]
    pub fn watch_tuned(&self, tuning: WatchTuning) -> Result<WatchHandle> {
        self.watch_with_hooks(tuning, WatchHooks::default())
    }

    /// [`Vault::watch_tuned`] with callbacks (the CLI re-syncs host skills
    /// after a pass that changed procedures).
    pub fn watch_with_hooks(&self, tuning: WatchTuning, hooks: WatchHooks) -> Result<WatchHandle> {
        let root = self.root().to_path_buf();
        let options = self.options.clone();
        let placement = self.placement.clone();
        let debounce = tuning.debounce.unwrap_or_else(|| self.debounce());
        let startup_timeout = tuning.startup_timeout;
        let stop = Arc::new(AtomicBool::new(false));
        let (events_tx, events) = mpsc::channel();
        let (backend_tx, backend_rx) = mpsc::channel::<DebounceEventResult>();
        let backend = backend_tx.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("wi-watch".into())
            .spawn(move || {
                let setup = Setup {
                    root,
                    options,
                    placement,
                    debounce,
                    tuning,
                    hooks,
                    backend: (backend_tx, backend_rx),
                };
                run(setup, thread_stop, events_tx);
            })
            .map_err(|e| StoreError::Watch(e.to_string()))?;
        let failed = match events.recv_timeout(startup_timeout) {
            Ok(WatchEvent::Ready) => None,
            Ok(WatchEvent::Error { message }) => Some(message),
            Ok(other) => Some(format!("unexpected {other:?}")),
            Err(e) => Some(e.to_string()),
        };
        if let Some(message) = failed {
            // The thread may still be opening the vault (waiting for a lock):
            // it checks this flag before it takes the lease or ingests, and
            // exits on its own. Joining here could block indefinitely.
            stop.store(true, Ordering::SeqCst);
            return Err(StoreError::Watch(message));
        }
        Ok(WatchHandle {
            stop,
            thread: Some(thread),
            events,
            backend,
        })
    }
}

/// Test-only watcher knobs ([`Vault::watch_tuned`]).
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct WatchTuning {
    /// Repair scan interval (default [`REPAIR_INTERVAL`]).
    pub repair_interval: Duration,
    /// Debounce override (default from config).
    pub debounce: Option<Duration>,
    /// While set, every repair scan fails.
    pub fail_repair: Arc<AtomicBool>,
    /// While set, every repair scan runs and then reports a failure, like
    /// a scan that publishes some paths before an error.
    pub fail_after_repair: Arc<AtomicBool>,
    /// How long `watch` waits for the thread to register (default
    /// [`STARTUP_TIMEOUT`]).
    pub startup_timeout: Duration,
    /// Least time between maintenance passes (default [`MAINTAIN_INTERVAL`]).
    pub maintain_interval: Duration,
    /// Interval of the full maintenance pass (default
    /// [`FULL_MAINTAIN_INTERVAL`]).
    pub full_maintain_interval: Duration,
}

/// Callback receiving each maintenance report.
pub type MaintainedHook = Box<dyn Fn(&MaintainReport) + Send + 'static>;

/// Callbacks the watcher runs on its own thread.
#[derive(Default)]
pub struct WatchHooks {
    /// Runs after every maintenance pass with its report.
    pub on_maintained: Option<MaintainedHook>,
}

impl std::fmt::Debug for WatchHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchHooks")
            .field("on_maintained", &self.on_maintained.is_some())
            .finish()
    }
}

impl Default for WatchTuning {
    fn default() -> Self {
        Self {
            repair_interval: REPAIR_INTERVAL,
            debounce: None,
            fail_repair: Arc::new(AtomicBool::new(false)),
            fail_after_repair: Arc::new(AtomicBool::new(false)),
            startup_timeout: STARTUP_TIMEOUT,
            maintain_interval: MAINTAIN_INTERVAL,
            full_maintain_interval: FULL_MAINTAIN_INTERVAL,
        }
    }
}

struct Setup {
    root: PathBuf,
    options: OpenOptions,
    placement: crate::vault::Placement,
    debounce: Duration,
    tuning: WatchTuning,
    hooks: WatchHooks,
    backend: (Sender<DebounceEventResult>, Receiver<DebounceEventResult>),
}

/// When the next maintenance pass runs (see the module docs).
#[derive(Debug)]
struct MaintainSchedule {
    interval: Duration,
    full_interval: Duration,
    /// Set by the first repair scan; no pass runs before it.
    armed: bool,
    last: Option<Instant>,
    last_full: Option<Instant>,
    /// Generation after the last pass.
    generation: Option<u64>,
    /// The last pass stopped at its budget: continue at the next tick.
    resume: bool,
}

/// What kind of pass is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaintainDue {
    No,
    /// Run when the generation changed since the last pass.
    IfChanged,
    Quick,
    Full,
}

impl MaintainSchedule {
    fn new(interval: Duration, full_interval: Duration) -> Self {
        Self {
            interval,
            full_interval,
            armed: false,
            last: None,
            last_full: None,
            generation: None,
            resume: false,
        }
    }

    fn due(&self, now: Instant) -> MaintainDue {
        if !self.armed {
            return MaintainDue::No;
        }
        let Some(last) = self.last else {
            return MaintainDue::Quick;
        };
        if self
            .last_full
            .is_none_or(|at| now >= at + self.full_interval)
            && now >= last
        {
            return MaintainDue::Full;
        }
        if self.resume {
            return MaintainDue::Quick;
        }
        if now >= last + self.interval {
            return MaintainDue::IfChanged;
        }
        MaintainDue::No
    }

    fn ran(&mut self, now: Instant, full: bool, generation: Option<u64>, complete: bool) {
        self.last = Some(now);
        if self.last_full.is_none() || (full && complete) {
            self.last_full = Some(now);
        }
        self.generation = generation;
        self.resume = !complete;
    }

    /// An unchanged vault: wait another interval before the next check.
    fn skipped(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

/// When the next repair scan runs: every interval, and soon after a
/// trigger (backend error, skipped directory event), but never twice
/// within [`REPAIR_COALESCE`].
#[derive(Debug)]
struct RepairSchedule {
    interval: Duration,
    last_run: Option<Instant>,
    next_at: Instant,
}

impl RepairSchedule {
    /// The first scan finds changes made while no watcher ran; it waits
    /// for the caller's start-up reconcile (see the module docs).
    fn new(interval: Duration, now: Instant) -> Self {
        Self {
            interval,
            last_run: None,
            next_at: now + REPAIR_START_DELAY.min(interval),
        }
    }

    fn trigger(&mut self, now: Instant) {
        let earliest = self
            .last_run
            .map_or(now, |at| (at + REPAIR_COALESCE).max(now));
        self.next_at = self.next_at.min(earliest);
    }

    fn due(&self, now: Instant) -> bool {
        now >= self.next_at
    }

    fn ran(&mut self, now: Instant, ok: bool) {
        self.last_run = Some(now);
        let wait = if ok {
            self.interval
        } else {
            self.interval.min(FLUSH_RETRY)
        };
        self.next_at = now + wait;
    }
}

/// The lease and whether a failed repair withdrew it. While withdrawn it
/// is neither refreshed nor reacquired, so one-shot commands ingest.
struct LeaseState {
    held: Option<crate::lease::Lease>,
    withdrawn: bool,
    refresh_at: Instant,
}

impl LeaseState {
    fn acquire(vault: &Vault, error: &dyn Fn(String)) -> Self {
        Self {
            held: crate::lease::Lease::acquire(&vault.history)
                .inspect_err(|e| error(format!("watcher lease: {e}")))
                .ok(),
            withdrawn: false,
            refresh_at: Instant::now() + crate::lease::LEASE_REFRESH,
        }
    }

    fn tick(&mut self, vault: &Vault, now: Instant, error: &dyn Fn(String)) {
        if self.withdrawn || now < self.refresh_at {
            return;
        }
        self.refresh_at = now + crate::lease::LEASE_REFRESH;
        match &mut self.held {
            Some(held) => {
                if let Err(e) = held.refresh() {
                    error(format!("watcher lease: {e}"));
                }
            }
            None => self.held = crate::lease::Lease::acquire(&vault.history).ok(),
        }
    }

    /// Dropping the lease removes its file when it is still ours.
    fn withdraw(&mut self) {
        self.held = None;
        self.withdrawn = true;
    }

    fn restore(&mut self, vault: &Vault, now: Instant) {
        if self.withdrawn {
            self.withdrawn = false;
            self.refresh_at = now;
            self.tick(vault, now, &|_| {});
        }
    }
}

/// The watcher loop's state, owned by its thread.
struct Watcher<'a> {
    vault: &'a Vault,
    events: &'a Sender<WatchEvent>,
    fail_repair: Arc<AtomicBool>,
    fail_after_repair: Arc<AtomicBool>,
    lease: LeaseState,
    schedule: RepairSchedule,
    maintain: MaintainSchedule,
    hooks: WatchHooks,
    /// Set while published records wait for a successful flush.
    retry_at: Option<Instant>,
}

impl Watcher<'_> {
    fn error(&self, message: String) {
        let _ = self.events.send(WatchEvent::Error { message });
    }

    fn flush(&mut self) {
        match self.vault.flush() {
            Ok(report) => {
                self.retry_at = None;
                let _ = self.events.send(WatchEvent::Flushed {
                    commits: report.commits.len(),
                });
            }
            Err(e) => {
                self.retry_at = Some(Instant::now() + FLUSH_RETRY);
                self.error(format!("flush: {e}"));
            }
        }
    }

    /// Whether the manifest holds records no flush has retired yet. A read
    /// error counts as pending, so the flush reports it.
    fn has_pending(&self) -> bool {
        wi_history::Manifest::snapshot_unlocked(&self.vault.history)
            .map_or(true, |snap| !snap.entries.is_empty())
    }

    /// One repair scan: ingest what the events missed and flush it
    /// like a batch. Any overall or per-path failure withdraws the lease
    /// until a scan succeeds without failed paths.
    /// Either way, pending records are flushed: a scan can publish
    /// some paths before it fails, and later scans see those paths as
    /// known, so whether this scan ingested something cannot decide.
    fn repair(&mut self) {
        let result = if self.fail_repair.load(Ordering::SeqCst) {
            Err(StoreError::Watch("injected repair failure".into()))
        } else {
            self.vault.repair_scan().and_then(|report| {
                if self.fail_after_repair.load(Ordering::SeqCst) {
                    Err(StoreError::Watch("injected failure after repair".into()))
                } else {
                    Ok(report)
                }
            })
        };
        let now = Instant::now();
        let ok = result.as_ref().is_ok_and(|report| report.failed.is_empty());
        self.schedule.ran(now, ok);
        if ok {
            self.lease.restore(self.vault, now);
        } else {
            self.lease.withdraw();
        }
        match result {
            Ok(report) => {
                for failure in &report.failed {
                    self.error(format!("repair: {}: {}", failure.path, failure.error));
                }
                for ingested in report.ingested {
                    let _ = self.events.send(WatchEvent::Ingest(Box::new(ingested)));
                }
            }
            Err(e) => {
                self.error(format!("repair: {e}"));
            }
        }
        if self.has_pending() {
            self.flush();
        }
        self.maintain.armed = true;
    }

    /// Run a maintenance pass when one is due (see the module docs).
    fn maybe_maintain(&mut self, now: Instant) {
        let due = self.maintain.due(now);
        if due == MaintainDue::No {
            return;
        }
        let generation = self.vault.generation().ok();
        if due == MaintainDue::IfChanged
            && generation.is_some()
            && generation == self.maintain.generation
        {
            self.maintain.skipped(now);
            return;
        }
        let full = due == MaintainDue::Full;
        let options = MaintainOptions {
            full,
            time_budget: Some(MAINTAIN_SLICE),
        };
        match self.vault.maintain(&options) {
            Ok(report) => {
                // A complete pass whose input missed a change remembers the
                // older generation, so the next check runs a corrective pass.
                let after = report
                    .complete
                    .then(|| self.vault.maintain_basis())
                    .flatten()
                    .or_else(|| self.vault.generation().ok());
                self.maintain
                    .ran(Instant::now(), full, after, report.complete);
                if let Some(hook) = &self.hooks.on_maintained {
                    hook(&report);
                }
            }
            Err(e) => {
                // Retried after another interval, changed or not.
                self.maintain.ran(Instant::now(), full, None, true);
                self.error(format!("maintain: {e}"));
            }
        }
    }

    fn step(&mut self, rx: &Receiver<DebounceEventResult>) -> bool {
        let now = Instant::now();
        let events = self.events;
        let error = |message: String| {
            let _ = events.send(WatchEvent::Error { message });
        };
        self.lease.tick(self.vault, now, &error);
        if self.retry_at.is_some_and(|at| now >= at) {
            self.flush();
        }
        if self.schedule.due(now) {
            self.repair();
        }
        self.maybe_maintain(Instant::now());
        let batch = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(batch)) => batch,
            Ok(Err(e)) => {
                self.error(e.to_string());
                self.schedule.trigger(Instant::now());
                return true;
            }
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        };
        let outcome = ingest_batch(self.vault, &batch, self.events);
        if outcome.skipped_directory {
            self.schedule.trigger(Instant::now());
        }
        if outcome.published {
            self.flush();
        }
        true
    }
}

fn run(setup: Setup, stop: Arc<AtomicBool>, events: Sender<WatchEvent>) {
    let error = |message: String| {
        let _ = events.send(WatchEvent::Error { message });
    };
    let vault = match Vault::open_placed(&setup.root, setup.options, setup.placement) {
        Ok(vault) => vault,
        Err(e) => return error(e.to_string()),
    };
    let (tx, rx) = setup.backend;
    let mut debouncer = match new_debouncer(setup.debounce, tx) {
        Ok(d) => d,
        Err(e) => return error(e.to_string()),
    };
    if let Err(e) = debouncer
        .watcher()
        .watch(&setup.root, RecursiveMode::Recursive)
    {
        return error(e.to_string());
    }
    // `watch` gave up waiting (its caller got an error): never take the
    // lease or ingest for a watcher nobody can stop.
    if stop.load(Ordering::SeqCst) {
        return;
    }
    let mut watcher = Watcher {
        vault: &vault,
        events: &events,
        fail_repair: setup.tuning.fail_repair,
        fail_after_repair: setup.tuning.fail_after_repair,
        lease: LeaseState::acquire(&vault, &error),
        schedule: RepairSchedule::new(setup.tuning.repair_interval, Instant::now()),
        maintain: MaintainSchedule::new(
            setup.tuning.maintain_interval,
            setup.tuning.full_maintain_interval,
        ),
        hooks: setup.hooks,
        retry_at: None,
    };
    let _ = events.send(WatchEvent::Ready);
    while !stop.load(Ordering::SeqCst) && watcher.step(&rx) {}
    // Orderly shutdown flushes whatever is pending, retry armed or not.
    if watcher.retry_at.is_some() || watcher.has_pending() {
        watcher.flush();
    }
}

/// What one debounced batch did.
struct BatchOutcome {
    /// Something was published and needs a flush.
    published: bool,
    /// A directory event was skipped, so changes inside it may be unseen.
    skipped_directory: bool,
}

/// File ingress for one debounced batch.
fn ingest_batch(
    vault: &Vault,
    batch: &[notify_debouncer_mini::DebouncedEvent],
    events: &Sender<WatchEvent>,
) -> BatchOutcome {
    let mut paths: Vec<String> = batch
        .iter()
        .filter_map(|e| relative(vault.root(), &e.path))
        .collect();
    if cfg!(target_os = "linux") {
        paths = expand_directories(vault.root(), paths);
    } else {
        paths.sort();
        paths.dedup();
    }
    let mut outcome = BatchOutcome {
        published: false,
        skipped_directory: false,
    };
    for rel in paths {
        if std::fs::symlink_metadata(vault.root().join(&rel)).is_ok_and(|m| m.is_dir()) {
            // An ignored directory needs no repair walk.
            outcome.skipped_directory |= !crate::paths::is_engine_internal(&rel)
                && !wi_index::ignored_dir(vault.root(), Path::new(&rel));
            continue;
        }
        match vault.ingest_path(&rel) {
            Ok(report) => {
                outcome.published |= matches!(
                    report.outcome,
                    IngestOutcome::Ingested { .. } | IngestOutcome::Missing
                );
                if !matches!(report.outcome, IngestOutcome::Skipped { .. }) {
                    let _ = events.send(WatchEvent::Ingest(Box::new(report)));
                }
            }
            Err(e) => {
                let _ = events.send(WatchEvent::Error {
                    message: format!("{rel}: {e}"),
                });
            }
        }
    }
    outcome
}

/// Inotify can report only the directory when files arrive before its
/// recursive watch is installed (including a populated directory rename).
/// Expand directory events before ingress so these files are not lost.
fn expand_directories(root: &Path, paths: Vec<String>) -> Vec<String> {
    let mut files = Vec::new();
    for rel in paths {
        let path = root.join(&rel);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            if !crate::paths::is_engine_internal(&rel)
                && !wi_index::ignored_dir(root, Path::new(&rel))
                && crate::paths::check_no_symlink(root, &rel).is_ok()
            {
                files.extend(crate::ingest::walk_files_in(root, &path));
            }
        } else {
            files.push(rel);
        }
    }
    files.sort();
    files.dedup();
    files
}

fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = match path.strip_prefix(root) {
        Ok(rel) => rel.to_path_buf(),
        // FSEvents may report a non-canonical prefix of the root.
        Err(_) => {
            let parent = path.parent()?.canonicalize().ok()?;
            parent
                .join(path.file_name()?)
                .strip_prefix(root)
                .ok()?
                .to_path_buf()
        }
    };
    let rel = rel.to_str()?;
    (!rel.is_empty()).then(|| rel.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_repair_withdraws_lease_flushes_success_and_recovers_only_when_clean() {
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let (vault, _) = Vault::init_with(
            dir.path().join("vault"),
            OpenOptions {
                index_path: Some(cache.path().join("index.sqlite3")),
            },
        )
        .unwrap();
        let terminal =
            "---\ntype: memory\nstatus: rejected\norigin:\n  kind: user\n---\nold body\n";
        std::fs::write(vault.root().join("a.md"), terminal).unwrap();
        assert!(vault.auto_ingest().unwrap().failed.is_empty());
        vault.flush().unwrap();

        let (events, rx) = mpsc::channel();
        let interval = Duration::from_secs(30);
        let mut watcher = Watcher {
            vault: &vault,
            events: &events,
            fail_repair: Arc::new(AtomicBool::new(false)),
            fail_after_repair: Arc::new(AtomicBool::new(false)),
            lease: LeaseState::acquire(&vault, &|e| panic!("{e}")),
            schedule: RepairSchedule::new(interval, Instant::now()),
            maintain: MaintainSchedule::new(MAINTAIN_INTERVAL, FULL_MAINTAIN_INTERVAL),
            hooks: WatchHooks::default(),
            retry_at: None,
        };
        let lease_file = vault.root().join(".git/wi/watcher-lease");
        assert!(lease_file.exists());
        assert_eq!(vault.auto_ingest().unwrap().scanned, 0);
        std::fs::write(
            vault.root().join("a.md"),
            terminal.replace("old body", "changed terminal body"),
        )
        .unwrap();
        std::fs::write(vault.root().join("z.md"), "successful repair\n").unwrap();

        // Run the real scan synchronously: terminal-note ingestion produces
        // Ok(report) with a failed path, while z.md is successfully published.
        watcher.repair();
        let emitted: Vec<_> = rx.try_iter().collect();
        assert!(
            emitted.iter().any(|event| matches!(
                event, WatchEvent::Error { message }
                    if message.contains("repair: a.md:") && message.contains("terminal")
            )),
            "{emitted:?}"
        );
        assert!(emitted.iter().any(|event| matches!(
            event, WatchEvent::Ingest(report)
                if report.path == "z.md" && matches!(report.outcome, IngestOutcome::Ingested { .. })
        )), "{emitted:?}");
        assert!(
            emitted.iter().any(|event| matches!(
                event, WatchEvent::Flushed { commits } if *commits > 0
            )),
            "{emitted:?}"
        );
        assert!(!watcher.has_pending());
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(vault.root())
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(git(&["show", "HEAD:z.md"]), "successful repair\n");
        assert!(git(&["log", "-1", "--format=%B", "--", "z.md"]).contains("Wi-Actor: watcher"));
        assert!(
            watcher.lease.withdrawn,
            "a partial report is a failed repair"
        );
        assert!(watcher.lease.held.is_none());
        assert!(!lease_file.exists());
        let ran = watcher.schedule.last_run.unwrap();
        assert_eq!(watcher.schedule.next_at, ran + FLUSH_RETRY);

        // Lease refresh cannot hide the failure; one-shot ingress resumes.
        watcher
            .lease
            .tick(&vault, Instant::now() + crate::lease::LEASE_REFRESH, &|e| {
                panic!("{e}")
            });
        assert!(!lease_file.exists());
        std::fs::write(vault.root().join("fallback.md"), "one-shot fallback\n").unwrap();
        let fallback = vault.auto_ingest().unwrap();
        assert!(fallback.scanned > 0);
        assert!(
            fallback
                .ingested
                .iter()
                .any(|report| report.path == "fallback.md")
        );
        vault.flush().unwrap();
        assert!(!lease_file.exists(), "only repair restores the lease");

        // Another partial repair must not restore a previously withdrawn lease.
        std::fs::write(
            vault.root().join("a.md"),
            terminal.replace("old body", "another terminal change"),
        )
        .unwrap();
        watcher.repair();
        assert!(rx.try_iter().any(|event| matches!(
            event, WatchEvent::Error { message } if message.contains("repair: a.md:")
        )));
        assert!(watcher.lease.withdrawn);
        assert!(!lease_file.exists());
        let ran = watcher.schedule.last_run.unwrap();
        assert_eq!(watcher.schedule.next_at, ran + FLUSH_RETRY);

        // The failing ingest's published bytes were flushed; unchanged paths
        // now produce a clean repair, which alone restores normal operation.
        watcher.repair();
        assert!(
            !rx.try_iter()
                .any(|event| matches!(event, WatchEvent::Error { .. }))
        );
        assert!(!watcher.lease.withdrawn);
        assert!(watcher.lease.held.is_some());
        assert!(lease_file.exists());
        let ran = watcher.schedule.last_run.unwrap();
        assert_eq!(watcher.schedule.next_at, ran + interval);
        assert_eq!(vault.auto_ingest().unwrap().scanned, 0);
    }

    #[test]
    fn repair_runs_at_start_then_every_interval() {
        let t0 = Instant::now();
        let interval = Duration::from_secs(30);
        let mut schedule = RepairSchedule::new(interval, t0);
        assert!(!schedule.due(t0), "not at once");
        assert!(
            schedule.due(t0 + REPAIR_START_DELAY),
            "first scan soon after start"
        );
        let short = RepairSchedule::new(Duration::from_millis(300), t0);
        assert!(
            short.due(t0 + Duration::from_millis(300)),
            "capped by the interval"
        );
        schedule.ran(t0, true);
        assert!(!schedule.due(t0 + interval - Duration::from_millis(1)));
        assert!(schedule.due(t0 + interval));
    }

    #[test]
    fn maintenance_waits_for_the_first_repair_then_runs_only_on_change() {
        let t0 = Instant::now();
        let minute = Duration::from_secs(60);
        let mut m = MaintainSchedule::new(15 * minute, 24 * 60 * minute);
        assert_eq!(m.due(t0), MaintainDue::No, "not before the first repair");
        m.armed = true;
        assert_eq!(m.due(t0), MaintainDue::Quick, "first pass after it");
        m.ran(t0, false, Some(7), true);
        assert_eq!(m.due(t0 + 14 * minute), MaintainDue::No);
        assert_eq!(m.due(t0 + 15 * minute), MaintainDue::IfChanged);
        m.skipped(t0 + 15 * minute);
        assert_eq!(
            m.due(t0 + 16 * minute),
            MaintainDue::No,
            "idle: one check per interval"
        );
        // A pass the budget stopped continues at once.
        m.ran(t0 + 20 * minute, false, Some(8), false);
        assert_eq!(m.due(t0 + 20 * minute), MaintainDue::Quick);
        m.ran(t0 + 20 * minute, false, Some(8), true);
        // The daily full pass.
        assert_eq!(m.due(t0 + 24 * 60 * minute), MaintainDue::Full);
        m.ran(t0 + 24 * 60 * minute, true, Some(8), true);
        assert_eq!(m.due(t0 + 24 * 60 * minute + minute), MaintainDue::No);
    }

    #[test]
    fn triggers_are_coalesced_but_never_postpone_a_due_scan() {
        let t0 = Instant::now();
        let mut schedule = RepairSchedule::new(Duration::from_secs(30), t0);
        schedule.ran(t0, true);
        // Right after a scan, a burst of triggers waits for the window.
        for ms in [10, 20, 500] {
            schedule.trigger(t0 + Duration::from_millis(ms));
        }
        assert!(!schedule.due(t0 + REPAIR_COALESCE - Duration::from_millis(1)));
        assert!(schedule.due(t0 + REPAIR_COALESCE));
        schedule.ran(t0 + REPAIR_COALESCE, true);
        // Long after a scan, a trigger is due at once.
        let later = t0 + Duration::from_secs(10);
        schedule.trigger(later);
        assert!(schedule.due(later));
    }

    #[test]
    fn a_failed_repair_is_retried_sooner() {
        let t0 = Instant::now();
        let mut schedule = RepairSchedule::new(Duration::from_secs(30), t0);
        schedule.ran(t0, false);
        assert!(!schedule.due(t0 + FLUSH_RETRY - Duration::from_millis(1)));
        assert!(schedule.due(t0 + FLUSH_RETRY));
        let mut short = RepairSchedule::new(Duration::from_millis(300), t0);
        short.ran(t0, false);
        assert!(short.due(t0 + Duration::from_millis(300)));
    }

    #[test]
    fn directory_event_finds_existing_children_with_vault_ignore_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("inbox/nested")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.ignore\n").unwrap();
        std::fs::write(root.join("inbox/.gitignore"), "local.md\n").unwrap();
        for name in [
            "inbox/a.md",
            "inbox/nested/b.md",
            "inbox/a.ignore",
            "inbox/local.md",
            "inbox/.a.md.wi-tmp-1-1",
            "elsewhere/c.md",
        ] {
            std::fs::write(root.join(name), "text").unwrap();
        }
        let paths = expand_directories(root, vec!["inbox".into(), "inbox/a.md".into()]);
        assert!(paths.contains(&"inbox/a.md".into()));
        assert!(paths.contains(&"inbox/nested/b.md".into()));
        assert_eq!(paths.iter().filter(|p| *p == "inbox/a.md").count(), 1);
        for excluded in [
            "inbox/a.ignore",
            "inbox/local.md",
            "inbox/.a.md.wi-tmp-1-1",
            "elsewhere/c.md",
        ] {
            assert!(!paths.iter().any(|p| p == excluded), "{paths:?}");
        }
    }
}
