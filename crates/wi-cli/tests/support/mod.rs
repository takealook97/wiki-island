//! Helpers shared by the CLI integration tests.
use std::process::{Command, Output};
use std::time::Duration;

const BUSY_TRIES: u32 = 20;
const BUSY_PAUSE: Duration = Duration::from_millis(50);

/// Runs `command` to completion. A freshly written executable can briefly fail with
/// `ExecutableFileBusy` on Linux while a concurrently forked test process still holds its
/// write descriptor, so that error is retried.
pub fn output(command: &mut Command) -> Output {
    let mut tries = 0;
    loop {
        match command.output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < BUSY_TRIES => {
                tries += 1;
                std::thread::sleep(BUSY_PAUSE);
            }
            result => return result.unwrap(),
        }
    }
}

/// Starts `command`, retrying `ExecutableFileBusy` like [`output`].
#[allow(dead_code)]
pub fn spawn(command: &mut Command) -> std::process::Child {
    let mut tries = 0;
    loop {
        match command.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < BUSY_TRIES => {
                tries += 1;
                std::thread::sleep(BUSY_PAUSE);
            }
            result => return result.unwrap(),
        }
    }
}
