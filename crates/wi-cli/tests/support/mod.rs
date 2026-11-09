//! Helpers shared by the CLI integration tests.
use std::io::{ErrorKind, Result};
use std::process::{Child, Command, Output};
use std::time::Duration;

/// Retries after the first attempt, so at most `BUSY_TRIES + 1` attempts in total.
#[allow(dead_code)]
pub const BUSY_TRIES: u32 = 20;
const BUSY_PAUSE: Duration = Duration::from_millis(50);

/// Calls `attempt` until it gives anything other than `ExecutableFileBusy`, retrying that error
/// at most [`BUSY_TRIES`] times with `pause` in between.
pub fn retry_busy<T>(pause: Duration, mut attempt: impl FnMut() -> Result<T>) -> Result<T> {
    let mut tries = 0;
    loop {
        match attempt() {
            Err(e) if e.kind() == ErrorKind::ExecutableFileBusy && tries < BUSY_TRIES => {
                tries += 1;
                std::thread::sleep(pause);
            }
            result => return result,
        }
    }
}

/// Runs `command` to completion. A freshly written executable can briefly fail with
/// `ExecutableFileBusy` on Linux while a concurrently forked test process still holds its
/// write descriptor, so that error is retried.
#[allow(dead_code)]
pub fn output(command: &mut Command) -> Output {
    retry_busy(BUSY_PAUSE, || command.output()).unwrap()
}

/// Starts `command`, retrying `ExecutableFileBusy` like [`output`].
#[allow(dead_code)]
pub fn spawn(command: &mut Command) -> Child {
    retry_busy(BUSY_PAUSE, || command.spawn()).unwrap()
}
