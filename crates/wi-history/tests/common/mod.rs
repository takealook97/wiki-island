#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;
use wi_history::{
    BatchMark, Blob, IngestRecord, IntentRecord, Manifest, PublisherLock, Seq, Vault, hash_bytes,
};

pub fn new_vault() -> (TempDir, Vault) {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::init(dir.path()).unwrap();
    (dir, vault)
}

pub fn intent(
    vault: &Vault,
    path: &str,
    bytes: &[u8],
    actor: &str,
    op_id: Option<&str>,
) -> IntentRecord {
    let old = std::fs::read(vault.root().join(path))
        .ok()
        .map(|b| hash_bytes(&b));
    IntentRecord {
        op_id: op_id.map(str::to_owned),
        request_digest: format!("digest-{}", path.replace('/', "_")),
        actor: actor.to_owned(),
        path: path.to_owned(),
        moved_from: None,
        old_hash: old,
        new_hash: hash_bytes(bytes),
        new_blob: Blob::write_durable(vault, bytes).unwrap(),
        revert_of: None,
        batch: None,
        losses: Vec::new(),
    }
}

pub fn write_file(vault: &Vault, path: &str, bytes: &[u8]) {
    let full = vault.root().join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, bytes).unwrap();
}

/// The engine write path in miniature: blob, intent, file, done.
pub fn save_record(vault: &Vault, rec: IntentRecord, bytes: &[u8]) -> Seq {
    let guard = PublisherLock::acquire(vault).unwrap();
    let path = rec.path.clone();
    let seq = Manifest::append_intent(vault, &guard, rec).unwrap();
    write_file(vault, &path, bytes);
    Manifest::append_done(vault, &guard, seq).unwrap();
    seq
}

pub fn save(vault: &Vault, path: &str, bytes: &[u8], actor: &str, op_id: Option<&str>) -> Seq {
    let rec = intent(vault, path, bytes, actor, op_id);
    save_record(vault, rec, bytes)
}

pub fn ingest(vault: &Vault, path: &str, bytes: &[u8], batch: Option<(&str, bool)>) -> Seq {
    write_file(vault, path, bytes);
    let guard = PublisherLock::acquire(vault).unwrap();
    let rec = IngestRecord {
        actor: "import".into(),
        path: path.into(),
        new_hash: hash_bytes(bytes),
        new_blob: Blob::write_durable(vault, bytes).unwrap(),
        batch: batch.map(|(id, last)| BatchMark {
            id: id.into(),
            last,
        }),
    };
    Manifest::append_ingest(vault, &guard, rec).unwrap()
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

/// First-parent commits, oldest first, as (id, full message), read by the git CLI.
pub fn commits(root: &Path) -> Vec<(String, String)> {
    if git(root, &["rev-list", "--all", "--max-count=1"])
        .trim()
        .is_empty()
    {
        return Vec::new();
    }
    git(
        root,
        &[
            "log",
            "--first-parent",
            "--reverse",
            "--format=%H%x00%B%x01",
        ],
    )
    .split('\x01')
    .filter_map(|chunk| {
        let chunk = chunk.trim_start_matches('\n');
        let (id, msg) = chunk.split_once('\0')?;
        Some((id.to_owned(), msg.to_owned()))
    })
    .collect()
}

pub fn trailer<'a>(message: &'a str, key: &str) -> Vec<&'a str> {
    message
        .lines()
        .filter_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(": ")))
        .collect()
}

/// Sequence ids named by a commit's `Wi-Seq` trailer.
pub fn seqs_of(message: &str) -> Vec<Seq> {
    trailer(message, "Wi-Seq")
        .iter()
        .flat_map(|v| v.split(' ').next().unwrap().split(','))
        .flat_map(|range| match range.split_once("..") {
            Some((a, b)) => (a.parse::<Seq>().unwrap()..=b.parse().unwrap()).collect::<Vec<_>>(),
            None => vec![range.parse().unwrap()],
        })
        .collect()
}

pub fn all_seqs(root: &Path) -> Vec<Seq> {
    commits(root).iter().flat_map(|(_, m)| seqs_of(m)).collect()
}

pub fn show(root: &Path, commit: &str, path: &str) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", &format!("{commit}:{path}")])
        .output()
        .unwrap();
    assert!(out.status.success());
    out.stdout
}

pub fn fsck(root: &Path) {
    git(root, &["fsck", "--strict", "--no-dangling"]);
}

pub fn testproc() -> Command {
    Command::new(env!("CARGO_BIN_EXE_wi-history-testproc"))
}
