//! Windows git transport (D14, D15): the transport tree runs in a
//! kill-on-close job, so a timeout ends git and every descendant, and an
//! HTTPS remote that asks for credentials fails without prompting.
//!
//! One test in its own binary because it sets `WI_SYNC_TIMEOUT_MS`, which
//! every sync in the process reads.
#![cfg(windows)]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use wi_store::{OpenOptions, Placement, StorageMode, Vault};

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn vault(temp: &Path) -> Vault {
    let options = OpenOptions {
        index_path: Some(temp.join("cache/index.sqlite3")),
    };
    let placement = Placement {
        data_root: Some(temp.join("data")),
        storage: Some(StorageMode::Git),
    };
    Vault::init_placed(temp.join("vault"), options, placement)
        .unwrap()
        .0
}

fn note(vault: &Vault, path: &str, text: &str) {
    std::fs::write(vault.root().join(path), text).unwrap();
    vault.ingest_path(path).unwrap();
    vault.flush().unwrap();
}

#[test]
fn transport_never_prompts_and_a_timeout_ends_the_whole_tree() {
    // An HTTPS remote that answers with an authentication challenge must
    // fail on its own, well before the transport timeout (60 s).
    let temp = TempDir::new().unwrap();
    let a = vault(temp.path());
    note(&a, "x.md", "x\n");
    a.sync_setup("https://github.com/takealook97/wi-no-such-private-repo-7f3c.git")
        .unwrap();
    let start = Instant::now();
    assert!(a.sync().is_err());
    assert!(
        start.elapsed() < Duration::from_secs(45),
        "an auth challenge took {:?}",
        start.elapsed()
    );
    drop(a);

    // A pre-receive hook (a grandchild of the transport git) that would
    // write a marker after 6 s. A 3 s timeout must end it with git.
    let temp = TempDir::new().unwrap();
    let remote = temp.path().join("remote.git");
    git(
        temp.path(),
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            remote.to_str().unwrap(),
        ],
    );
    let a = vault(temp.path());
    a.sync_setup(remote.to_str().unwrap()).unwrap();
    note(&a, "base.md", "base\n");
    a.sync().unwrap();
    let marker = temp.path().join("hook-finished");
    let marker_sh = marker.to_str().unwrap().replace('\\', "/");
    std::fs::write(
        remote.join("hooks/pre-receive"),
        format!("#!/bin/sh\nsleep 6\necho done > '{marker_sh}'\nexit 1\n"),
    )
    .unwrap();
    note(&a, "late.md", "late\n");
    // SAFETY: this binary runs this single test, so no other thread reads
    // the environment concurrently.
    unsafe { std::env::set_var("WI_SYNC_TIMEOUT_MS", "3000") };
    let start = Instant::now();
    let result = a.sync();
    let elapsed = start.elapsed();
    assert!(result.is_err(), "{result:?}");
    assert!(elapsed < Duration::from_secs(6), "timeout took {elapsed:?}");
    std::thread::sleep(Duration::from_secs(8));
    assert!(!marker.exists(), "the hook outlived the transport timeout");
}
