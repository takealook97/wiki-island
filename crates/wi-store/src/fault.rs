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
