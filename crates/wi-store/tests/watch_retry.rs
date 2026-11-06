//! Own test binary: `WI_FAULT=flush_once` is process-wide.

mod common;

use std::time::{Duration, Instant};

use common::{Fixture, git};
use wi_store::{FLUSH_RETRY, WatchEvent};

#[test]
fn a_failed_watcher_flush_is_retried_without_new_events() {
    let (_fx, vault) = Fixture::new();
    // SAFETY: the only test in this binary; set before the watcher thread
    // starts, read by `Vault::flush`.
    unsafe { std::env::set_var("WI_FAULT", "flush_once") };
    let handle = vault.watch().unwrap();
    std::fs::write(vault.root().join("retry.md"), "retry me\n").unwrap();
    let deadline = Instant::now() + FLUSH_RETRY + Duration::from_secs(10);
    let (mut failed_at, mut flushed_at) = (None, None);
    while Instant::now() < deadline && flushed_at.is_none() {
        match handle.events().recv_timeout(Duration::from_millis(100)) {
            Ok(WatchEvent::Error { message }) if message.contains("injected flush") => {
                failed_at = Some(Instant::now());
            }
            Ok(WatchEvent::Flushed { commits }) if commits > 0 => {
                flushed_at = Some(Instant::now());
            }
            _ => {}
        }
    }
    handle.stop();
    let failed_at = failed_at.expect("the first flush failed");
    let flushed_at = flushed_at.expect("the retry committed without another event");
    // The retry waits for the timer, not for an unrelated event.
    assert!(
        flushed_at - failed_at >= FLUSH_RETRY - Duration::from_millis(200),
        "{:?}",
        flushed_at - failed_at
    );
    let log = git(
        vault.root(),
        &["log", "--format=%s%n%b", "-1", "--", "retry.md"],
    );
    assert!(log.contains("Wi-Actor: watcher"), "{log}");
}
