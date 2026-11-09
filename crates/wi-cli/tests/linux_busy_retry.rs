//! The `ExecutableFileBusy` retry used when tests run freshly written scripts on Linux.
mod support;
use std::cell::Cell;
use std::io::{Error, ErrorKind};
use std::time::Duration;
use support::{BUSY_TRIES, retry_busy};

const NO_PAUSE: Duration = Duration::ZERO;

#[test]
fn busy_attempts_are_retried_until_one_succeeds() {
    let calls = Cell::new(0);
    let result = retry_busy(NO_PAUSE, || {
        calls.set(calls.get() + 1);
        if calls.get() <= 5 {
            Err(Error::from(ErrorKind::ExecutableFileBusy))
        } else {
            Ok("ran")
        }
    });
    assert_eq!(result.unwrap(), "ran");
    assert_eq!(calls.get(), 6);
}

#[test]
fn a_program_that_stays_busy_gives_up_after_a_bounded_number_of_attempts() {
    let calls = Cell::new(0);
    let result: std::io::Result<()> = retry_busy(NO_PAUSE, || {
        calls.set(calls.get() + 1);
        Err(Error::from(ErrorKind::ExecutableFileBusy))
    });
    assert_eq!(result.unwrap_err().kind(), ErrorKind::ExecutableFileBusy);
    assert_eq!(calls.get(), BUSY_TRIES + 1);
}

#[test]
fn other_errors_are_returned_at_once() {
    let calls = Cell::new(0);
    let result: std::io::Result<()> = retry_busy(NO_PAUSE, || {
        calls.set(calls.get() + 1);
        Err(Error::from(ErrorKind::NotFound))
    });
    assert_eq!(result.unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(calls.get(), 1);
}
