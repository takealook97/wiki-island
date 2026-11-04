//! Fault-injection hooks for crash and race tests, driven by `WI_FAULT`.
//!
//! - `WI_FAULT=after_commit_before_retire`: a flush aborts the process after
//!   its commits and ref are durable and before it retires the prefix.
//! - `WI_FAULT=after_ref_before_barrier`: a flush or revert aborts the
//!   process after the branch moved and before the ref passed its barrier.
//! - `WI_FAULT=pause_after_snapshot:<dir>`: a flush creates `<dir>/ready`
//!   after taking its snapshot and waits (up to 60 s) for `<dir>/go`.
//! - `WI_FAULT=pause_before_advance:<dir>`: the same handshake after a flush
//!   wrote its commit objects and before it moves the branch.
//!
//! Unset in normal operation; any other value is ignored. Builds without
//! the `fault-injection` feature (every release build) ignore `WI_FAULT`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

const VAR: &str = "WI_FAULT";

fn value() -> Option<String> {
    if !cfg!(feature = "fault-injection") {
        return None;
    }
    std::env::var(VAR).ok()
}

pub(crate) fn after_commit_before_retire() {
    if value().as_deref() == Some("after_commit_before_retire") {
        std::process::abort();
    }
}

/// Between a visible ref update and its barrier. Unit tests can make it
/// return an error instead (an fsync failure after publication).
pub(crate) fn after_ref_before_barrier() -> crate::error::Result<()> {
    if value().as_deref() == Some("after_ref_before_barrier") {
        std::process::abort();
    }
    #[cfg(test)]
    if FAIL_REF_BARRIER.with(std::cell::Cell::get) {
        return Err(crate::error::HistoryError::Injected(
            "after_ref_before_barrier",
        ));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_REF_BARRIER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn pause_after_snapshot() {
    pause("pause_after_snapshot:");
}

pub(crate) fn pause_before_advance() {
    pause("pause_before_advance:");
}

fn pause(prefix: &str) {
    let Some(dir) = value().and_then(|v| v.strip_prefix(prefix).map(PathBuf::from)) else {
        return;
    };
    // Best effort: a failed handshake only skips the pause.
    if std::fs::write(dir.join("ready"), b"").is_err() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while !dir.join("go").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}
