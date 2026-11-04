//! Barrier-order tests. Power loss cannot be simulated, so these
//! assert, on the in-process barrier trace, that every object, ref and
//! directory a retirement depends on passed its barrier first.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::durable::trace::{self, Kind};
use crate::{Blob, FlushReport, Flusher, IntentRecord, Manifest, PublisherLock, Vault, hash_bytes};

type Trace = Vec<(Kind, PathBuf)>;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn save(vault: &Vault, path: &str, bytes: &[u8], moved_from: Option<&str>) {
    let guard = PublisherLock::acquire(vault).unwrap();
    let rec = IntentRecord {
        op_id: None,
        request_digest: "d".into(),
        actor: "cli".into(),
        path: path.into(),
        moved_from: moved_from.map(str::to_owned),
        old_hash: None,
        new_hash: hash_bytes(bytes),
        new_blob: Blob::write_durable(vault, bytes).unwrap(),
        revert_of: None,
        batch: None,
        losses: Vec::new(),
    };
    let seq = Manifest::append_intent(vault, &guard, rec).unwrap();
    let full = vault.root().join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, bytes).unwrap();
    if let Some(from) = moved_from {
        std::fs::remove_file(vault.root().join(from)).unwrap();
    }
    Manifest::append_done(vault, &guard, seq).unwrap();
}

fn flush_traced(vault: &Vault) -> (FlushReport, Trace) {
    let (report, events) = trace::capture(|| Flusher::flush(vault));
    (report.unwrap(), events)
}

fn position(events: &Trace, kind: Kind, path: &Path) -> Option<usize> {
    events.iter().position(|(k, p)| *k == kind && p == path)
}

/// Index of the manifest rename that retires records.
fn retirement(vault: &Vault, events: &Trace) -> usize {
    position(events, Kind::Rename, &vault.wi_dir().join("manifest.log"))
        .expect("the flush retired records")
}

fn loose(vault: &Vault, oid: &str) -> PathBuf {
    vault
        .git_dir()
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..])
}

fn assert_before(events: &Trace, kind: Kind, path: &Path, limit: usize) {
    let at = position(events, kind, path)
        .unwrap_or_else(|| panic!("no {kind:?} barrier for {}", path.display()));
    assert!(at < limit, "{kind:?} {} after retirement", path.display());
}

fn new_vault() -> (tempfile::TempDir, Vault) {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::init(dir.path()).unwrap();
    (dir, vault)
}

#[test]
fn init_barriers_git_metadata_before_git_becomes_visible() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    let (vault, events) = trace::capture(|| Vault::init(&root));
    vault.unwrap();
    let rename = position(&events, Kind::Rename, &root.join(".git")).expect("staged rename");
    let staged = root.join(".wiki-island-init.tmp").join(".git");
    for file in ["HEAD", "config"] {
        assert_before(&events, Kind::File, &staged.join(file), rename);
    }
    for sub in ["refs/heads", "refs/tags", "refs", "objects", ""] {
        let path = if sub.is_empty() {
            staged.clone()
        } else {
            staged.join(sub)
        };
        assert_before(&events, Kind::Dir, &path, rename);
    }
    let canonical = root.canonicalize().unwrap();
    let root_barrier = events
        .iter()
        .rposition(|(k, p)| *k == Kind::Dir && p == &canonical)
        .unwrap();
    assert!(root_barrier > rename, "the new .git entry is made durable");
    let marker = canonical.join(".git/wi/init-complete");
    let marked = position(&events, Kind::Rename, &marker).expect("init marker");
    assert!(root_barrier < marked, "the marker follows the root barrier");
    assert!(!root.join(".wiki-island-init.tmp").exists());
}

/// `.git` became visible but the process died (or the root barrier
/// failed) before the root barrier. The retry must complete that barrier
/// before it accepts and retires a write.
#[test]
fn init_retry_after_visible_rename_completes_the_root_barrier() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    let staging = root.join(".wiki-island-init.tmp");
    // The exact state after `init`'s pre-root-barrier steps.
    gix::init(&staging).unwrap();
    crate::durable::barrier_tree(&staging.join(".git"), crate::Durability::Standard).unwrap();
    std::fs::rename(staging.join(".git"), root.join(".git")).unwrap();

    let canonical = root.canonicalize().unwrap();
    let (vault, events) = trace::capture(|| {
        let vault = Vault::init(&root).unwrap();
        save(&vault, "a.md", b"a", None);
        let report = Flusher::flush(&vault).unwrap();
        assert_eq!(report.retired_upto, 1);
        vault
    });
    let root_barrier = position(&events, Kind::Dir, &canonical).expect("root barrier on retry");
    assert!(root_barrier < retirement(&vault, &events));
    let marker = vault.wi_dir().join("init-complete");
    let marked = position(&events, Kind::Rename, &marker).expect("marker written");
    assert!(root_barrier < marked);
    assert!(!staging.exists());

    // Once marked, opening again issues no root barrier.
    let (reopened, events) = trace::capture(|| Vault::open(&root));
    reopened.unwrap();
    assert_eq!(position(&events, Kind::Dir, &canonical), None);
}

/// A vault without the marker (made by an older build or another tool)
/// gets the root barrier on its first open.
#[test]
fn open_without_marker_barriers_the_root_once() {
    let (_dir, vault) = new_vault();
    let marker = vault.wi_dir().join("init-complete");
    assert!(marker.is_file());
    std::fs::remove_file(&marker).unwrap();
    let (reopened, events) = trace::capture(|| Vault::open(vault.root()));
    reopened.unwrap();
    let root_barrier = position(&events, Kind::Dir, vault.root()).expect("root barrier");
    let marked = position(&events, Kind::Rename, &marker).expect("marker rewritten");
    assert!(root_barrier < marked);
    assert!(marker.is_file());
}

#[test]
fn stale_init_staging_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join(".wiki-island-init.tmp").join(".git");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("HEAD"), b"half").unwrap();
    let vault = Vault::init(dir.path()).unwrap();
    assert!(vault.git_dir().join("HEAD").is_file());
    assert!(!dir.path().join(".wiki-island-init.tmp").exists());
}

#[test]
fn ordinary_flush_barriers_objects_then_ref_then_retires() {
    let (_dir, vault) = new_vault();
    save(&vault, "notes/a.md", b"a", None);
    let (report, events) = flush_traced(&vault);
    assert_eq!(report.retired_upto, 1);
    let retire = retirement(&vault, &events);
    let commit = report.commits[0].commit.clone();
    let commit_at = position(&events, Kind::File, &loose(&vault, &commit)).unwrap();
    let root_tree = git(vault.root(), &["rev-parse", "HEAD^{tree}"]);
    let notes_tree = git(vault.root(), &["rev-parse", "HEAD:notes"]);
    let ref_path = vault
        .git_dir()
        .join("refs/heads")
        .join(git(vault.root(), &["symbolic-ref", "--short", "HEAD"]));
    let ref_at = position(&events, Kind::File, &ref_path).unwrap();
    assert_before(&events, Kind::File, &loose(&vault, &root_tree), ref_at);
    assert_before(&events, Kind::File, &loose(&vault, &notes_tree), ref_at);
    assert!(commit_at < ref_at && ref_at < retire);
    assert_before(
        &events,
        Kind::Dir,
        &vault.git_dir().join("refs/heads"),
        retire,
    );
    assert_before(&events, Kind::Dir, &vault.git_dir().join("refs"), retire);
}

#[test]
fn move_barriers_the_changed_source_tree_before_retirement() {
    let (_dir, vault) = new_vault();
    save(&vault, "src/a.md", b"a", None);
    save(&vault, "src/b.md", b"b", None);
    Flusher::flush(&vault).unwrap();
    let old_src = git(vault.root(), &["rev-parse", "HEAD:src"]);
    save(&vault, "archive/src/a.md", b"a", Some("src/a.md"));
    let (report, events) = flush_traced(&vault);
    assert_eq!(report.commits.len(), 1);
    let new_src = git(vault.root(), &["rev-parse", "HEAD:src"]);
    assert_ne!(old_src, new_src, "the source directory tree changed");
    assert!(loose(&vault, &new_src).is_file());
    let retire = retirement(&vault, &events);
    assert_before(&events, Kind::File, &loose(&vault, &new_src), retire);
    let archive = git(vault.root(), &["rev-parse", "HEAD:archive/src"]);
    assert_before(&events, Kind::File, &loose(&vault, &archive), retire);
}

#[test]
fn ref_published_without_its_barrier_is_barriered_before_retirement() {
    let (_dir, vault) = new_vault();
    save(&vault, "a.md", b"a", None);
    save(&vault, "b.md", b"b", None);
    crate::fault::FAIL_REF_BARRIER.with(|f| f.set(true));
    let failed = Flusher::flush(&vault);
    crate::fault::FAIL_REF_BARRIER.with(|f| f.set(false));
    assert!(matches!(failed, Err(crate::HistoryError::Injected(_))));
    let head = git(vault.root(), &["rev-parse", "HEAD"]);
    assert!(!head.is_empty(), "the ref is visible");

    let (report, events) = flush_traced(&vault);
    assert!(report.commits.is_empty());
    assert_eq!(report.already_committed, vec![1, 2]);
    let retire = retirement(&vault, &events);
    let branch = git(vault.root(), &["symbolic-ref", "HEAD"]);
    let ref_path = vault.git_dir().join(&branch);
    assert_before(&events, Kind::File, &ref_path, retire);
    assert_before(
        &events,
        Kind::Dir,
        &vault.git_dir().join("refs/heads"),
        retire,
    );
    assert_before(&events, Kind::File, &loose(&vault, &head), retire);
    let parent = git(vault.root(), &["rev-parse", "HEAD~1"]);
    assert_before(&events, Kind::File, &loose(&vault, &parent), retire);
}

#[test]
fn nested_branch_barriers_every_new_ref_directory() {
    let (_dir, vault) = new_vault();
    git(
        vault.root(),
        &["symbolic-ref", "HEAD", "refs/heads/new/deep/branch"],
    );
    save(&vault, "a.md", b"a", None);
    let (report, events) = flush_traced(&vault);
    assert_eq!(report.retired_upto, 1);
    let retire = retirement(&vault, &events);
    let refs = vault.git_dir().join("refs");
    assert_before(
        &events,
        Kind::File,
        &refs.join("heads/new/deep/branch"),
        retire,
    );
    for dir in ["heads/new/deep", "heads/new", "heads", ""] {
        let path = if dir.is_empty() {
            refs.clone()
        } else {
            refs.join(dir)
        };
        assert_before(&events, Kind::Dir, &path, retire);
    }
}

#[test]
fn every_append_barriers_its_blob_before_the_record() {
    let (_dir, vault) = new_vault();
    let ((), events) = trace::capture(|| save(&vault, "a.md", b"bytes", None));
    let blob = git(vault.root(), &["hash-object", "a.md"]);
    let manifest = vault.wi_dir().join("manifest.log");
    let blob_at = position(&events, Kind::File, &loose(&vault, &blob)).unwrap();
    let first_append = position(&events, Kind::File, &manifest).unwrap();
    assert!(blob_at < first_append);
    let appends = events
        .iter()
        .filter(|(k, p)| *k == Kind::File && p == &manifest)
        .count();
    assert_eq!(appends, 2, "intent and done each pass a barrier");
}
