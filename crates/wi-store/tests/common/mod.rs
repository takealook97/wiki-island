#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

pub struct Fixture {
    pub dir: TempDir,
    pub cache: TempDir,
}

impl Fixture {
    pub fn new() -> (Self, Vault) {
        let dir = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let fixture = Self { dir, cache };
        let (vault, _) = Vault::init_with(fixture.root_input(), fixture.options()).unwrap();
        (fixture, vault)
    }

    pub fn root_input(&self) -> PathBuf {
        self.dir.path().join("vault")
    }

    pub fn options(&self) -> OpenOptions {
        OpenOptions {
            index_path: Some(self.cache.path().join("index.sqlite3")),
        }
    }

    pub fn open(&self) -> Vault {
        Vault::open_with(self.root_input(), self.options()).unwrap()
    }
}

pub fn write(vault: &Vault, path: &str, content: &str, ingress: Ingress) -> wi_store::WriteResult {
    vault
        .write(WriteRequest {
            path: path.into(),
            content: content.as_bytes().to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress,
        })
        .unwrap()
}

pub fn git(root: &Path, args: &[&str]) -> String {
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

pub fn manifest_text(vault: &Vault) -> String {
    String::from_utf8_lossy(
        &std::fs::read(vault.root().join(".git/wi/manifest.log")).unwrap_or_default(),
    )
    .into_owned()
}

/// The owner's deletion of `path` is committed (a `Wi-Deleted` trailer, the
/// path gone from the branch tip, a clean `git status`) and left no work
/// request.
pub fn assert_deletion_committed(vault: &Vault, path: &str) {
    let root = vault.root();
    let log = git(root, &["log", "--format=%B", "-1"]);
    assert!(log.contains(&format!("Wi-Deleted: {path}")), "{log}");
    assert!(git(root, &["ls-tree", "-r", "--name-only", "HEAD", "--", path]).is_empty());
    assert_eq!(git(root, &["status", "--porcelain", "--", path]), "");
    assert!(
        !vault
            .work_requests()
            .unwrap()
            .iter()
            .any(|w| w.path == path),
        "no work request for an owner deletion"
    );
}
