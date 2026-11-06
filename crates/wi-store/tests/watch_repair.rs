//! The watcher's repair scan and lease withdrawal.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{Fixture, git};
use wi_store::{Vault, WatchTuning};

fn lease_file(vault: &Vault) -> PathBuf {
    vault.root().join(".git/wi/watcher-lease")
}

fn committed_by_watcher(vault: &Vault, path: &str) -> bool {
    git(vault.root(), &["log", "--format=%s%n%b", "-1", "--", path]).contains("Wi-Actor: watcher")
}

fn eventually(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    done()
}

fn tuning(repair_interval: Duration) -> WatchTuning {
    WatchTuning {
        repair_interval,
        // Long enough to make a file unreadable before its event arrives.
        debounce: Some(Duration::from_millis(800)),
        fail_repair: Arc::new(AtomicBool::new(false)),
        fail_after_repair: Arc::new(AtomicBool::new(false)),
        ..WatchTuning::default()
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// The recheck reproduction: the event for a save arrives while its
/// directory is unreadable, so the event is lost; the repair scan records
/// the note once the directory is readable again.
#[cfg(unix)]
#[test]
fn a_save_missed_in_an_unreadable_directory_reaches_history_by_repair() {
    let (_fx, vault) = Fixture::new();
    let locked = vault.root().join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    let handle = vault
        .watch_tuned(tuning(Duration::from_millis(300)))
        .unwrap();
    std::fs::write(locked.join("missed.md"), "missed save\n").unwrap();
    set_mode(&locked, 0o000);
    std::thread::sleep(Duration::from_secs(2));
    set_mode(&locked, 0o755);
    let recorded = eventually(Duration::from_secs(5), || {
        committed_by_watcher(&vault, "locked/missed.md")
    });
    handle.stop();
    assert!(recorded, "the repair scan recorded the missed save");
}

fn ingress_cache(fx: &Fixture) -> PathBuf {
    std::fs::read_dir(fx.cache.path())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ingress-") && n.ends_with(".stat"))
        })
        .expect("a repair scan stored the ingress cache")
}

#[cfg(unix)]
#[test]
fn a_backend_error_triggers_an_immediate_repair() {
    let (fx, vault) = Fixture::new();
    let handle = vault
        .watch_tuned(tuning(Duration::from_secs(3600)))
        .unwrap();
    // The first scheduled scan waits `REPAIR_START_DELAY` (10 s), and a
    // scan of an unchanged vault leaves a current cache alone. So once the
    // cache file is removed, only a triggered repair scan (a full compare
    // without a cache) stores it again.
    let has_cache = || {
        std::fs::read_dir(fx.cache.path())
            .unwrap()
            .any(|e| e.unwrap().path().extension().is_some_and(|x| x == "stat"))
    };
    if has_cache() {
        std::fs::remove_file(ingress_cache(&fx)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!has_cache(), "no repair without a trigger");
    handle.inject_backend_error("synthetic backend error");
    let repaired = eventually(Duration::from_secs(3), has_cache);
    handle.stop();
    assert!(repaired, "the backend error started a repair scan");
}

#[test]
fn a_failed_repair_withdraws_the_lease_until_a_repair_succeeds() {
    let (_fx, vault) = Fixture::new();
    let tuning = tuning(Duration::from_millis(300));
    let fail = tuning.fail_repair.clone();
    let handle = vault.watch_tuned(tuning).unwrap();
    assert!(lease_file(&vault).exists(), "the watcher holds the lease");
    assert_eq!(vault.auto_ingest().unwrap().scanned, 0, "lease held");

    fail.store(true, Ordering::SeqCst);
    assert!(
        eventually(Duration::from_secs(3), || !lease_file(&vault).exists()),
        "a failed repair removes the lease"
    );
    std::fs::write(vault.root().join("while-failing.md"), "one-shot\n").unwrap();
    let report = vault.auto_ingest().unwrap();
    assert!(report.scanned > 0, "one-shot ingress resumes: {report:?}");
    // A maintenance pass of the watcher may hold its one-commit batch
    // open; the record commits with the next flush after the pass.
    assert!(eventually(Duration::from_secs(5), || {
        vault.flush().unwrap();
        committed_by_watcher(&vault, "while-failing.md")
    }));
    // Not refreshed while the failure is outstanding (refresh every 2 s).
    std::thread::sleep(wi_store::LEASE_REFRESH + Duration::from_millis(500));
    assert!(!lease_file(&vault).exists(), "no refresh while failing");

    fail.store(false, Ordering::SeqCst);
    assert!(
        eventually(wi_store::FLUSH_RETRY, || lease_file(&vault).exists()),
        "a successful repair restores the lease"
    );
    let before = std::fs::read_to_string(lease_file(&vault)).unwrap();
    assert!(
        eventually(wi_store::LEASE_REFRESH * 2, || {
            std::fs::read_to_string(lease_file(&vault)).unwrap_or_default() != before
        }),
        "the restored lease is refreshed again"
    );
    assert_eq!(vault.auto_ingest().unwrap().scanned, 0, "lease held again");
    handle.stop();
    assert!(!lease_file(&vault).exists());
}

/// A repair that publishes a note and then
/// fails must still get it committed. Later scans already know the note
/// from the manifest, so only the pending records can decide the flush.
#[test]
fn a_repair_that_publishes_then_fails_still_commits_by_stop() {
    let (_fx, vault) = Fixture::new();
    let mut tuning = tuning(Duration::from_secs(3600));
    // Long enough that the repair, not the file event, ingests the note.
    tuning.debounce = Some(Duration::from_secs(3));
    let fail_after = tuning.fail_after_repair.clone();
    let handle = vault.watch_tuned(tuning).unwrap();
    // The first scheduled repair waits `REPAIR_START_DELAY` (10 s), so the
    // triggered repair below is the one that publishes and fails.
    std::thread::sleep(Duration::from_millis(1200));
    fail_after.store(true, Ordering::SeqCst);
    std::fs::write(vault.root().join("a.md"), "published by a failing repair\n").unwrap();
    handle.inject_backend_error("synthetic backend error");
    assert!(
        eventually(Duration::from_secs(3), || !lease_file(&vault).exists()),
        "the failing repair ran and withdrew the lease"
    );
    fail_after.store(false, Ordering::SeqCst);
    // The failed repair is retried after FLUSH_RETRY; let it and the
    // delayed file event pass, then stop.
    let committed = eventually(wi_store::FLUSH_RETRY + Duration::from_secs(2), || {
        committed_by_watcher(&vault, "a.md")
    });
    handle.stop();
    assert!(
        committed || committed_by_watcher(&vault, "a.md"),
        "a.md reached history before or at stop"
    );
}
