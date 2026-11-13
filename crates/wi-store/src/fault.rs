//! Crash hooks for tests, driven by `WI_FAULT=<step>`. Each named step
//! aborts the process at that point of the write path:
//!
//! - `before_intent_fsync`: temp file and blob are durable, no record yet.
//!   A process kill cannot lose bytes the kernel accepted, so "before the
//!   intent fsync" is simulated as "before the intent append".
//! - `after_intent_before_swap`: the intent is durable, the file is old.
//! - `after_swap_before_done`: the new file is in place, no `done` yet.
//! - `after_done`: acknowledged, before the index update.
//!
//! `WI_FAULT=index_update` makes the index update fail (no abort), so the
//! result carries `index: pending`. `WI_FAULT=flush_once` makes the first
//! `Vault::flush` of the process fail (watcher retry tests). Unset in normal
//! operation; builds without the `fault-injection` feature (every release
//! build) ignore it.
//!
//! Pause points fix the interleaving of two threads in tests. A test calls
//! [`pause_point`] to register a name; the first thread that reaches
//! `pause(name)` marks the point reached and blocks until the test calls
//! [`PausePoint::release`], drops the handle, or 60 seconds pass. After the
//! release, later calls with that name pass through. Names without a
//! registration pass through at once. Without the feature, `pause` is an
//! empty function and none of this exists.

const VAR: &str = "WI_FAULT";

fn value() -> Option<String> {
    if !cfg!(feature = "fault-injection") {
        return None;
    }
    std::env::var(VAR).ok()
}

pub(crate) fn abort_at(step: &str) {
    if value().as_deref() == Some(step) {
        std::process::abort();
    }
}

pub(crate) fn fail_index() -> bool {
    value().as_deref() == Some("index_update")
}

pub(crate) fn fail_flush_once() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static FAILED: AtomicBool = AtomicBool::new(false);
    value().as_deref() == Some("flush_once") && !FAILED.swap(true, Ordering::SeqCst)
}

/// Blocks at the named pause point while a test holds it. No-op without the
/// `fault-injection` feature.
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
#[allow(dead_code)]
pub(crate) fn pause(_: &str) {}

#[cfg(feature = "fault-injection")]
pub use pause_impl::{PausePoint, pause_point};

/// Calls the internal `pause` hook from an integration test.
#[cfg(feature = "fault-injection")]
#[doc(hidden)]
pub fn pause_for_test(name: &str) {
    pause_impl::pause(name);
}

#[cfg(feature = "fault-injection")]
#[allow(unused_imports)]
pub(crate) use pause_impl::pause;

#[cfg(feature = "fault-injection")]
mod pause_impl {
    use std::collections::HashMap;
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
    use std::time::Duration;

    const PAUSE_LIMIT: Duration = Duration::from_secs(60);

    #[derive(Default)]
    struct PointState {
        reached: bool,
        released: bool,
    }

    #[derive(Default)]
    struct Point {
        state: Mutex<PointState>,
        cv: Condvar,
    }

    type Registry = Mutex<HashMap<&'static str, Arc<Point>>>;

    fn registry() -> &'static Registry {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY.get_or_init(Registry::default)
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A registered pause point. Dropping it releases waiters and
    /// unregisters the name.
    pub struct PausePoint {
        name: &'static str,
        point: Arc<Point>,
    }

    /// Registers `name`. Panics when the name is already registered.
    pub fn pause_point(name: &'static str) -> PausePoint {
        let mut points = lock(registry());
        assert!(
            !points.contains_key(name),
            "pause point {name} is already registered"
        );
        let point = Arc::new(Point::default());
        points.insert(name, Arc::clone(&point));
        PausePoint { name, point }
    }

    impl PausePoint {
        /// Waits until a thread reached the point; false on timeout.
        pub fn wait_reached(&self, timeout: Duration) -> bool {
            let state = lock(&self.point.state);
            let (state, _) = self
                .point
                .cv
                .wait_timeout_while(state, timeout, |s| !s.reached)
                .unwrap_or_else(|e| e.into_inner());
            state.reached
        }

        /// Lets blocked and later threads pass. Repeated calls do nothing.
        pub fn release(&self) {
            lock(&self.point.state).released = true;
            self.point.cv.notify_all();
        }
    }

    impl Drop for PausePoint {
        fn drop(&mut self) {
            self.release();
            lock(registry()).remove(self.name);
        }
    }

    pub(crate) fn pause(name: &str) {
        let Some(point) = lock(registry()).get(name).cloned() else {
            return;
        };
        let mut state = lock(&point.state);
        if state.released {
            return;
        }
        state.reached = true;
        point.cv.notify_all();
        let _ = point
            .cv
            .wait_timeout_while(state, PAUSE_LIMIT, |s| !s.released)
            .unwrap_or_else(|e| e.into_inner());
    }
}
