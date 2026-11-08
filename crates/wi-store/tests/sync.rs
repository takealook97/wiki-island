//! Remote synchronization against a temporary bare repository only.
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
use wi_store::{OpenOptions, Placement, StorageMode, Vault, WorkClose};

struct Device {
    root: PathBuf,
    data: PathBuf,
    index: PathBuf,
}
impl Device {
    fn at(dir: &Path, name: &str) -> Self {
        Self {
            root: dir.join(name),
            data: dir.join(format!("{name}-data")),
            index: dir.join(format!("{name}-cache/index.sqlite3")),
        }
    }
    fn options(&self) -> OpenOptions {
        OpenOptions {
            index_path: Some(self.index.clone()),
        }
    }
    fn placement(&self) -> Placement {
        Placement {
            data_root: Some(self.data.clone()),
            storage: Some(StorageMode::Git),
        }
    }
    fn init(&self) -> Vault {
        Vault::init_placed(&self.root, self.options(), self.placement())
            .unwrap()
            .0
    }
    fn open(&self) -> Vault {
        Vault::open_placed(&self.root, self.options(), self.placement()).unwrap()
    }
}
struct Pair {
    _temp: TempDir,
    remote: PathBuf,
    a: Device,
    b: Device,
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
fn write(vault: &Vault, path: &str, text: &str) {
    std::fs::write(vault.root().join(path), text).unwrap();
    vault.ingest_path(path).unwrap();
    vault.flush().unwrap();
}
impl Pair {
    fn new() -> Self {
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
        let a = Device::at(temp.path(), "a");
        let b = Device::at(temp.path(), "b");
        let vault = a.init();
        vault.sync_setup(remote.to_str().unwrap()).unwrap();
        write(&vault, "base.md", "base\n");
        vault.sync().unwrap();
        git(
            temp.path(),
            &["clone", remote.to_str().unwrap(), b.root.to_str().unwrap()],
        );
        b.open().sync_setup(remote.to_str().unwrap()).unwrap();
        Self {
            _temp: temp,
            remote,
            a,
            b,
        }
    }
}

#[test]
fn fast_forward_applies_exact_bytes_and_advances_the_read_generation() {
    let p = Pair::new();
    let a = p.a.open();
    let b = p.b.open();
    let generation = b.generation().unwrap();
    write(
        &a,
        "new.md",
        "---\nunknown: [one, two] # preserve\n---\nnew\n",
    );
    a.sync().unwrap();
    let report = b.sync().unwrap();
    assert!(report.changed.contains(&"new.md".into()));
    assert!(report.conflicts.is_empty());
    assert_eq!(
        a.read("new.md").unwrap().bytes,
        b.read("new.md").unwrap().bytes
    );
    assert!(b.generation().unwrap() > generation);
    assert_eq!(
        git(&p.a.root, &["rev-parse", "HEAD"]),
        git(&p.b.root, &["rev-parse", "HEAD"])
    );
    assert!(git(&p.b.root, &["status", "--porcelain"]).is_empty());
    assert!(b.sync().unwrap().changed.is_empty());
}

#[test]
fn diverged_disjoint_changes_merge_with_local_first_parent() {
    let p = Pair::new();
    let a = p.a.open();
    let b = p.b.open();
    write(&a, "a.md", "a\n");
    write(&b, "b.md", "b\n");
    let local = git(&p.b.root, &["rev-parse", "HEAD"]).trim().to_owned();
    a.sync().unwrap();
    let report = b.sync().unwrap();
    assert!(report.conflicts.is_empty());
    assert_eq!(git(&p.b.root, &["rev-parse", "HEAD^1"]).trim(), local);
    assert_eq!(b.read("a.md").unwrap().bytes, b"a\n");
    assert_eq!(b.read("b.md").unwrap().bytes, b"b\n");
    a.sync().unwrap();
    assert_eq!(
        git(&p.a.root, &["rev-parse", "HEAD"]),
        git(&p.b.root, &["rev-parse", "HEAD"])
    );
}

#[test]
fn same_note_conflict_keeps_local_and_commits_a_remote_copy() {
    let p = Pair::new();
    let a = p.a.open();
    let b = p.b.open();
    write(&a, "base.md", "remote\n");
    write(&b, "base.md", "local\n");
    a.sync().unwrap();
    let report = b.sync().unwrap();
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(b.read("base.md").unwrap().bytes, b"local\n");
    assert_eq!(b.read(&report.conflicts[0]).unwrap().bytes, b"remote\n");
    let search = b
        .search(
            &wi_store::Query::text("remote"),
            wi_store::SearchOptions { catch_up: true },
        )
        .unwrap();
    assert!(
        search.hits.is_empty(),
        "a conflict copy must not become another active note"
    );
    assert!(
        b.work_requests()
            .unwrap()
            .iter()
            .any(|r| r.kind == wi_store::WorkKind::ConflictCopy)
    );
    a.sync().unwrap();
    assert_eq!(a.read("base.md").unwrap().bytes, b"local\n");
    assert_eq!(a.read(&report.conflicts[0]).unwrap().bytes, b"remote\n");
    assert!(
        !String::from_utf8(b.read("base.md").unwrap().bytes)
            .unwrap()
            .contains("<<<<<<<")
    );
}

#[test]
fn delete_against_edit_keeps_the_edit_in_a_copy_and_the_deletion_in_history() {
    let p = Pair::new();
    let a = p.a.open();
    let b = p.b.open();
    write(&a, "base.md", "remote edit\n");
    std::fs::remove_file(p.b.root.join("base.md")).unwrap();
    b.auto_ingest().unwrap();
    b.flush().unwrap();
    a.sync().unwrap();
    let report = b.sync().unwrap();
    assert!(!p.b.root.join("base.md").exists());
    assert_eq!(
        b.read(&report.conflicts[0]).unwrap().bytes,
        b"remote edit\n"
    );
    assert_eq!(git(&p.b.root, &["show", "HEAD^2:base.md"]), "remote edit\n");
}

#[test]
fn edit_against_remote_delete_keeps_the_local_note() {
    let p = Pair::new();
    let a = p.a.open();
    let b = p.b.open();
    std::fs::remove_file(p.a.root.join("base.md")).unwrap();
    a.auto_ingest().unwrap();
    a.flush().unwrap();
    write(&b, "base.md", "local edit\n");
    a.sync().unwrap();
    let report = b.sync().unwrap();
    assert_eq!(b.read("base.md").unwrap().bytes, b"local edit\n");
    assert_eq!(report.conflicts.len(), 1);
    assert!(
        b.work_requests()
            .unwrap()
            .iter()
            .any(|r| r.detail.contains("remote deletion"))
    );
}

#[test]
fn work_notes_have_disjoint_device_paths_and_foreign_items_are_read_only() {
    let p = Pair::new();
    let a = p.a.open();
    let b = p.b.open();
    let first = a.work_open_conflict("base.md", "base.md", "check").unwrap();
    let second = b.work_open_conflict("base.md", "base.md", "check").unwrap();
    assert_ne!(first, second);
    a.flush().unwrap();
    b.flush().unwrap();
    a.sync().unwrap();
    let report = b.sync().unwrap();
    assert!(report.conflicts.is_empty());
    a.sync().unwrap();
    let items = b.work_list().unwrap();
    assert!(items.iter().any(|i| i.path == first && !i.actionable));
    assert!(items.iter().any(|i| i.path == second && i.actionable));
    let closed = WorkClose {
        path: first.clone(),
        outcome: wi_lifecycle::Outcome::Adopted,
        limits: None,
        reopen_trigger: None,
        reopen_detail: None,
    };
    let bytes = a.read(&first).unwrap().bytes;
    assert!(b.work_close(&closed).is_err());
    assert_eq!(b.read(&first).unwrap().bytes, bytes);
    b.sweep().unwrap();
    assert_eq!(b.read(&first).unwrap().bytes, bytes);
}

#[test]
fn agent_provenance_survives_round_trip_unchanged() {
    let p = Pair::new();
    let a = p.a.open();
    let b = p.b.open();
    let bytes = b"---\ntype: memory\nkind: fact\nstatus: candidate\nsummary: Source claim\norigin:\n  kind: agent\n  locator: test\nnoted_by: agent\ncustom: preserved # comment\n---\nSource claim.\n";
    std::fs::write(p.a.root.join("agent.md"), bytes).unwrap();
    a.auto_ingest().unwrap();
    a.flush().unwrap();
    let sent = a.read("agent.md").unwrap().bytes;
    a.sync().unwrap();
    b.sync().unwrap();
    assert_eq!(b.read("agent.md").unwrap().bytes, sent);
    b.sync().unwrap();
    a.sync().unwrap();
    assert_eq!(a.read("agent.md").unwrap().bytes, sent);
    assert!(String::from_utf8(sent).unwrap().contains("kind: agent"));
}

#[test]
fn unavailable_remote_backs_off_and_explicit_retry_recovers() {
    let p = Pair::new();
    let a = p.a.open();
    a.sync_setup(p.remote.with_extension("missing").to_str().unwrap())
        .unwrap();
    assert!(a.sync().is_err());
    let status = a.sync_status().unwrap();
    assert_eq!(status.failures, 1);
    assert!(status.next_attempt_ms > status.failing_since_ms.unwrap());
    assert!(a.sync_background().unwrap().deferred);
    a.sync_setup(p.remote.to_str().unwrap()).unwrap();
    assert!(a.sync().unwrap().pushed);
    assert_eq!(a.sync_status().unwrap().failures, 0);
}

#[test]
fn rejected_push_is_retried_without_losing_local_history() {
    use std::os::unix::fs::PermissionsExt;
    let p = Pair::new();
    let a = p.a.open();
    write(&a, "pending.md", "pending\n");
    let hook = p.remote.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let head = git(&p.a.root, &["rev-parse", "HEAD"]);
    assert!(a.sync().is_err());
    assert_eq!(git(&p.a.root, &["rev-parse", "HEAD"]), head);
    std::fs::remove_file(hook).unwrap();
    assert!(a.sync().unwrap().pushed);
    p.b.open().sync().unwrap();
    assert_eq!(p.b.open().read("pending.md").unwrap().bytes, b"pending\n");
}

#[test]
fn files_over_fifty_megabytes_stay_local_and_get_one_work_request() {
    let p = Pair::new();
    let a = p.a.open();
    let file = std::fs::File::create(p.a.root.join("big.dat")).unwrap();
    file.set_len(50 * 1024 * 1024 + 1).unwrap();
    let report = a.sync().unwrap();
    assert_eq!(report.excluded, vec!["big.dat"]);
    assert_eq!(
        std::fs::metadata(p.a.root.join("big.dat")).unwrap().len(),
        50 * 1024 * 1024 + 1
    );
    assert!(
        git(&p.a.root, &["ls-tree", "-r", "--name-only", "HEAD"])
            .lines()
            .all(|p| p != "big.dat")
    );
    a.sync().unwrap();
    assert_eq!(
        a.work_requests()
            .unwrap()
            .iter()
            .filter(|r| r.path == "big.dat")
            .count(),
        1
    );
}

#[test]
fn a_day_of_continuous_failure_reports_once_and_backoff_caps_at_an_hour() {
    let p = Pair::new();
    let a = p.a.open();
    a.sync_setup(p.remote.with_extension("missing").to_str().unwrap())
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let state = serde_json::json!({"configured":true,"failures":100,"failing_since_ms":now-25*60*60*1000,"next_attempt_ms":0,"last_success_ms":null,"health_reported":false,"health":null});
    std::fs::write(
        p.a.root.join(".git/wi/sync-state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    assert!(a.sync().is_err());
    let status = a.sync_status().unwrap();
    assert!(status.health.is_some());
    assert!(status.health_reported);
    assert!(status.next_attempt_ms >= now + 3_600_000);
    assert!(status.next_attempt_ms < now + 3_605_000);
    assert!(a.sync().is_err());
    assert!(a.sync_status().unwrap().health.is_none());
    a.sync_setup(p.remote.to_str().unwrap()).unwrap();
    a.sync().unwrap();
    assert!(!a.sync_status().unwrap().health_reported);
}

#[test]
fn the_separate_background_thread_pushes_on_start_and_stops_cleanly() {
    let p = Pair::new();
    let a = p.a.open();
    write(&a, "background.md", "background\n");
    let sync = a.start_sync().unwrap().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let names = git(&p.remote, &["ls-tree", "-r", "--name-only", "main"]);
        if names.lines().any(|p| p == "background.md") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background startup sync did not publish"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    sync.stop();
    assert!(a.sync_status().unwrap().last_success_ms.is_some());
}

#[test]
fn operations_from_both_merge_parents_replay_after_index_deletion() {
    let p = Pair::new();
    let a = p.a.open();
    let b = p.b.open();
    let request = |path: &str, op: &str| wi_store::WriteRequest {
        path: path.into(),
        content: b"one operation\n".to_vec(),
        expected_checksum: None,
        op_id: Some(op.into()),
        ingress: wi_store::Ingress::Mcp,
    };
    a.write(request("operation-a.md", "operation-a")).unwrap();
    a.flush().unwrap();
    b.write(request("operation-b.md", "operation-b")).unwrap();
    b.flush().unwrap();
    a.sync().unwrap();
    b.sync().unwrap();
    a.sync().unwrap();
    drop(a);
    drop(b);
    for device in [&p.a, &p.b] {
        std::fs::remove_file(&device.index).unwrap();
        let vault = device.open();
        for (path, op) in [
            ("operation-a.md", "operation-a"),
            ("operation-b.md", "operation-b"),
        ] {
            let result = vault.write(request(path, op)).unwrap();
            assert!(result.replayed, "{op} must replay on either device");
        }
    }
}

#[test]
fn an_empty_local_remote_keeps_its_default_branch_for_ordinary_clones() {
    let temp = TempDir::new().unwrap();
    let remote = temp.path().join("remote.git");
    git(
        temp.path(),
        &[
            "init",
            "--bare",
            "--initial-branch=notes",
            remote.to_str().unwrap(),
        ],
    );
    let a = Device::at(temp.path(), "first");
    let b = Device::at(temp.path(), "second");
    let vault = a.init();
    vault.sync_setup(remote.to_str().unwrap()).unwrap();
    write(&vault, "note.md", "cloned\n");
    vault.sync().unwrap();
    git(
        temp.path(),
        &["clone", remote.to_str().unwrap(), b.root.to_str().unwrap()],
    );
    let cloned = b.open();
    assert_eq!(cloned.read("note.md").unwrap().bytes, b"cloned\n");
    assert_eq!(
        git(&b.root, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "notes"
    );
}

#[test]
fn a_busy_git_staging_index_does_not_hold_sync_or_subsequent_saves_open() {
    let p = Pair::new();
    let a = p.a.open();
    let b = p.b.open();
    write(&a, "download.md", "downloaded\n");
    a.sync().unwrap();
    let lock = p.b.root.join(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    b.sync().unwrap();
    assert_eq!(b.read("download.md").unwrap().bytes, b"downloaded\n");
    assert!(!p.b.root.join(".git/wi/sync-intent.json").exists());
    write(&b, "later.md", "later save\n");
    assert!(b.flush().unwrap().index_warning.is_some());
    std::fs::remove_file(lock).unwrap();
    assert!(b.flush().unwrap().index_warning.is_none());
    assert!(git(&p.b.root, &["status", "--porcelain"]).is_empty());
    b.sync().unwrap();
    a.sync().unwrap();
    assert_eq!(a.read("later.md").unwrap().bytes, b"later save\n");
}

#[test]
fn a_different_vault_identity_is_refused_without_replacing_the_remote_config() {
    let p = Pair::new();
    let other = Device::at(p._temp.path(), "unrelated");
    let vault = other.init();
    assert_ne!(vault.vault_id(), p.a.open().vault_id());
    vault.sync_setup(p.remote.to_str().unwrap()).unwrap();
    write(&vault, "private.md", "another vault\n");
    let before = git(&p.remote, &["rev-parse", "main"]);
    assert!(vault.sync().is_err());
    assert_eq!(git(&p.remote, &["rev-parse", "main"]), before);
    assert_eq!(vault.read("private.md").unwrap().bytes, b"another vault\n");
    assert!(p.a.open().sync().unwrap().pushed);
}

#[test]
fn an_oversized_engine_write_stays_in_the_folder_and_out_of_history() {
    let p = Pair::new();
    let a = p.a.open();
    let result = a
        .write(wi_store::WriteRequest {
            path: "large.md".into(),
            content: vec![b'x'; 50 * 1024 * 1024 + 1],
            expected_checksum: None,
            op_id: None,
            ingress: wi_store::Ingress::Cli,
        })
        .unwrap();
    a.flush().unwrap();
    assert!(std::fs::metadata(p.a.root.join("large.md")).unwrap().len() > 50 * 1024 * 1024);
    // A replacement against the returned checksum also stays local.
    a.write(wi_store::WriteRequest {
        path: "large.md".into(),
        content: vec![b'y'; 50 * 1024 * 1024 + 2],
        expected_checksum: Some(result.checksum),
        op_id: None,
        ingress: wi_store::Ingress::Cli,
    })
    .unwrap();
    a.auto_ingest().unwrap();
    a.flush().unwrap();
    assert!(a.sync().unwrap().pushed);
    for root in [&p.a.root, &p.remote] {
        let all = git(root, &["rev-list", "--objects", "--all"]);
        assert!(all.lines().all(|l| !l.ends_with(" large.md")), "{all}");
    }
    assert!(
        std::fs::read_to_string(p.a.root.join(".git/info/exclude"))
            .unwrap()
            .lines()
            .any(|l| l == "/large.md")
    );
    a.sync().unwrap();
    assert_eq!(
        a.work_requests()
            .unwrap()
            .iter()
            .filter(|r| r.path == "large.md")
            .count(),
        1
    );
}

#[test]
fn an_oversized_ancestor_committed_elsewhere_blocks_upload_without_rewriting() {
    let p = Pair::new();
    let a = p.a.open();
    let remote_before = git(&p.remote, &["rev-parse", "main"]);
    let file = std::fs::File::create(p.a.root.join("large.dat")).unwrap();
    file.set_len(50 * 1024 * 1024 + 1).unwrap();
    // Another git tool committed the file before the engine saw it.
    git(&p.a.root, &["add", "-f", "large.dat"]);
    git(
        &p.a.root,
        &[
            "-c",
            "user.name=other",
            "-c",
            "user.email=other@example.invalid",
            "commit",
            "-qm",
            "other tool",
        ],
    );
    let saved = git(&p.a.root, &["rev-parse", "HEAD"]);
    assert!(a.sync().is_err(), "exclusion cannot hide an ancestral blob");
    assert_eq!(git(&p.remote, &["rev-parse", "main"]), remote_before);
    assert!(
        git(
            &p.a.root,
            &["merge-base", "--is-ancestor", saved.trim(), "HEAD"]
        )
        .is_empty()
    );
    a.sync().unwrap_err();
    assert_eq!(
        a.work_requests()
            .unwrap()
            .iter()
            .filter(
                |request| request.kind == wi_store::WorkKind::RecoveryProblem
                    && request.path == "large.dat"
            )
            .count(),
        1
    );
}

#[test]
fn a_note_that_grows_past_fifty_megabytes_is_never_deleted_on_any_device() {
    let p = Pair::new();
    let a = p.a.open();
    write(&a, "big.md", "important small note\n");
    a.sync().unwrap();
    let b = p.b.open();
    b.sync().unwrap();
    assert_eq!(
        std::fs::read_to_string(p.b.root.join("big.md")).unwrap(),
        "important small note\n"
    );
    let committed = git(&p.a.root, &["rev-parse", "HEAD:big.md"]);
    let file = std::fs::OpenOptions::new()
        .append(true)
        .open(p.a.root.join("big.md"))
        .unwrap();
    file.set_len(50 * 1024 * 1024 + 1).unwrap();
    drop(file);
    let report = a.sync().unwrap();
    assert_eq!(report.excluded, ["big.md"]);
    a.reconcile().unwrap();
    a.flush().unwrap();
    a.sync().unwrap();
    // The tip keeps the last committed version; the new bytes stay local.
    assert_eq!(git(&p.a.root, &["rev-parse", "HEAD:big.md"]), committed);
    assert_eq!(git(&p.remote, &["rev-parse", "main:big.md"]), committed);
    assert!(std::fs::metadata(p.a.root.join("big.md")).unwrap().len() > 50 * 1024 * 1024);
    b.sync().unwrap();
    assert_eq!(
        std::fs::read_to_string(p.b.root.join("big.md")).unwrap(),
        "important small note\n"
    );
    assert_eq!(
        a.work_requests()
            .unwrap()
            .iter()
            .filter(|r| r.path == "big.md")
            .count(),
        1
    );
    assert!(b.work_requests().unwrap().iter().all(|r| r.path != "big.md"));
}

#[test]
fn an_ingested_file_over_fifty_megabytes_before_sync_setup_never_blocks_sync() {
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
    let a = Device::at(temp.path(), "a");
    let vault = a.init();
    write(&vault, "note.md", "note\n");
    let file = std::fs::File::create(a.root.join("big.dat")).unwrap();
    file.set_len(50 * 1024 * 1024 + 1).unwrap();
    drop(file);
    vault.ingest_path("big.dat").unwrap();
    vault.flush().unwrap();
    let all = git(&a.root, &["rev-list", "--objects", "--all"]);
    assert!(all.lines().all(|l| !l.ends_with(" big.dat")), "{all}");
    vault.sync_setup(remote.to_str().unwrap()).unwrap();
    assert!(vault.sync().unwrap().pushed);
    let pushed = git(&remote, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(pushed.lines().any(|l| l == "note.md"), "{pushed}");
    assert!(pushed.lines().all(|l| l != "big.dat"), "{pushed}");
}

#[test]
fn a_remote_file_name_this_device_cannot_create_is_skipped_and_sync_continues() {
    let p = Pair::new();
    let long = format!("{}.md", "a".repeat(300));
    // A careless peer commits a name no local file system can hold.
    let blob = {
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(&p.remote)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(b"x\n").unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned()
    };
    let listing = git(&p.remote, &["ls-tree", "main^{tree}"]);
    let tree = {
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(&p.remote)
            .arg("mktree")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        let input = format!("{listing}100644 blob {blob}\t{long}\n");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned()
    };
    let parent = git(&p.remote, &["rev-parse", "main"]);
    let commit = git(
        &p.remote,
        &[
            "-c",
            "user.name=peer",
            "-c",
            "user.email=peer@example.invalid",
            "commit-tree",
            &tree,
            "-p",
            parent.trim(),
            "-m",
            "long name",
        ],
    );
    git(&p.remote, &["update-ref", "refs/heads/main", commit.trim()]);
    let b = p.b.open();
    write(&b, "later.md", "later\n");
    assert!(b.sync().unwrap().pushed);
    let names = git(&p.remote, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(names.lines().any(|l| l == "later.md"), "{names}");
    assert!(names.lines().any(|l| l == long), "kept for other devices");
    b.reconcile().unwrap();
    b.flush().unwrap();
    write(&b, "after.md", "after\n");
    assert!(b.sync().unwrap().pushed);
    let names = git(&p.remote, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(names.lines().any(|l| l == "after.md"), "{names}");
    assert!(names.lines().any(|l| l == long), "never deleted: {names}");
    assert_eq!(
        b.work_requests()
            .unwrap()
            .iter()
            .filter(|r| r.path == long)
            .count(),
        1
    );
    b.doctor().unwrap();
    let a = p.a.open();
    a.sync().unwrap();
    assert_eq!(
        std::fs::read_to_string(p.a.root.join("later.md")).unwrap(),
        "later\n"
    );
}
