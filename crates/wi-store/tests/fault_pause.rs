//! In-process pause points of the `fault-injection` feature.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use wi_store::fault::{pause_for_test, pause_point};

const WAIT: Duration = Duration::from_secs(5);

#[test]
fn pause_point_blocks_until_release() {
    let point = pause_point("test_blocks_until_release");
    let passed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&passed);
    let worker = thread::spawn(move || {
        pause_for_test("test_blocks_until_release");
        flag.store(true, Ordering::SeqCst);
    });

    assert!(point.wait_reached(WAIT));
    thread::sleep(Duration::from_millis(50));
    assert!(!passed.load(Ordering::SeqCst));

    point.release();
    worker.join().unwrap();
    assert!(passed.load(Ordering::SeqCst));
}

#[test]
fn pause_point_unregistered_name_passes() {
    let start = Instant::now();
    pause_for_test("test_never_registered");
    assert!(start.elapsed() < WAIT);
}

#[test]
fn pause_point_drop_releases_waiter() {
    let point = pause_point("test_drop_releases");
    let worker = thread::spawn(|| pause_for_test("test_drop_releases"));
    assert!(point.wait_reached(WAIT));
    drop(point);
    worker.join().unwrap();

    let again = pause_point("test_drop_releases");
    assert!(!again.wait_reached(Duration::from_millis(50)));
}

#[test]
fn pause_point_second_call_after_release_passes() {
    let point = pause_point("test_second_call");
    let worker = thread::spawn(|| pause_for_test("test_second_call"));
    assert!(point.wait_reached(WAIT));
    point.release();
    point.release();
    worker.join().unwrap();

    let start = Instant::now();
    pause_for_test("test_second_call");
    assert!(start.elapsed() < WAIT);
}

#[test]
#[should_panic(expected = "pause point test_duplicate is already registered")]
fn pause_point_duplicate_registration_panics() {
    let _first = pause_point("test_duplicate");
    let _second = pause_point("test_duplicate");
}
