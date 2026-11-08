//! Sync and migration crash probes. All git remotes, data and homes are temporary.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    _temp: tempfile::TempDir,
    base: PathBuf,
    remote: PathBuf,
}
impl Env {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().to_path_buf();
        let remote = base.join("remote.git");
        let e = Self {
            _temp: temp,
            base,
            remote,
        };
        e.git(
            &e.base,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                e.remote.to_str().unwrap(),
            ],
        );
        e.ok("a", &["init"]);
        e.ok("a", &["sync", "setup", e.remote.to_str().unwrap()]);
        e.ok("a", &["write", "base.md", "--content", "base\n"]);
        e.ok("a", &["sync"]);
        e.git(
            &e.base,
            &[
                "clone",
                e.remote.to_str().unwrap(),
                e.base.join("b").to_str().unwrap(),
            ],
        );
        e.ok("b", &["sync", "setup", e.remote.to_str().unwrap()]);
        e
    }
    fn command(&self, device: &str, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_wi"));
        c.args(args)
            .arg("--json")
            .env("WI_VAULT", self.base.join(device))
            .env("HOME", self.base.join(format!("{device}-home")))
            .env("WI_DATA_DIR", self.base.join(format!("{device}-data")))
            .env("WI_CACHE_DIR", self.base.join(format!("{device}-cache")))
            .env_remove("WI_FAULT");
        c
    }
    fn run(&self, device: &str, args: &[&str]) -> Output {
        self.command(device, args).output().unwrap()
    }
    fn ok(&self, device: &str, args: &[&str]) -> Value {
        let out = self.run(device, args);
        assert!(out.status.success(), "{device} {args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn git(&self, root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("HOME", self.base.join("git-home"))
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
    fn edit(&self, device: &str, content: &str) {
        let note = self.ok(device, &["read", "base.md"]);
        self.ok(
            device,
            &[
                "write",
                "base.md",
                "--content",
                content,
                "--expected-checksum",
                note["checksum"].as_str().unwrap(),
            ],
        );
    }
}

#[test]
fn every_sync_crash_step_reopens_and_converges_without_losing_either_version() {
    for step in [
        "sync_after_flush",
        "sync_after_fetch",
        "sync_after_intent",
        "sync_after_file",
        "sync_before_ref",
        "sync_after_ref",
        "sync_after_generation",
        "sync_after_index",
        "sync_before_push",
        "sync_after_push",
    ] {
        let e = Env::new();
        e.edit("a", "remote version\n");
        e.ok("a", &["write", "extra.md", "--content", "extra\n"]);
        e.ok("a", &["sync"]);
        e.edit("b", "local version\n");
        let crashed = e
            .command("b", &["sync"])
            .env("WI_FAULT", step)
            .output()
            .unwrap();
        assert!(!crashed.status.success(), "{step} should abort");
        // Opening alone replays any durable local intent without transport.
        let opened = e.ok("b", &["read", "base.md"]);
        assert!(
            e.git(
                &e.base.join("b"),
                &["for-each-ref", "--format=%(refname)", "refs/wi-sync/"]
            )
            .is_empty(),
            "dead transport refs are retired on open"
        );
        assert!(
            opened["content"]
                .as_str()
                .is_some_and(|s| s.ends_with("local version\n")),
            "{step}: {opened}"
        );
        e.ok("b", &["sync"]);
        e.ok("a", &["sync"]);
        let names = e.git(&e.base.join("b"), &["ls-tree", "-r", "--name-only", "HEAD"]);
        let copy = names
            .lines()
            .find(|p| p.starts_with(".base.md.wi-conflict-sync-"))
            .expect("remote version kept");
        assert!(
            std::fs::read(e.base.join("b").join(copy))
                .unwrap()
                .ends_with(b"remote version\n"),
            "{step}"
        );
        assert!(
            std::fs::read(e.base.join("b/extra.md"))
                .unwrap()
                .ends_with(b"extra\n"),
            "{step}"
        );
        assert!(
            !e.base.join("b/.git/wi/sync-intent.json").exists(),
            "{step}"
        );
        e.ok("b", &["work", "list"]);
        assert_eq!(
            e.git(&e.base.join("a"), &["rev-parse", "HEAD"]),
            e.git(&e.base.join("b"), &["rev-parse", "HEAD"]),
            "{step}"
        );
    }
}

#[test]
fn flat_work_notes_migrate_with_the_existing_move_recovery_protocol() {
    for step in [
        "after_intent_before_swap",
        "after_swap_before_done",
        "after_done",
    ] {
        let e = Env::new();
        let item = wi_lifecycle::WorkItem::new(
            wi_lifecycle::WorkKind::Recheck,
            "base.md",
            vec![],
            "check",
            "read the source",
            wi_lifecycle::Trigger::Session,
            wi_lifecycle::parse_time("2026-01-01T00:00:00Z").unwrap(),
        );
        let flat = item.path(".wiki-island/work");
        std::fs::create_dir_all(e.base.join("a/.wiki-island/work")).unwrap();
        std::fs::write(e.base.join("a").join(&flat), item.render()).unwrap();
        let crashed = e
            .command("a", &["work", "list"])
            .env("WI_FAULT", step)
            .output()
            .unwrap();
        assert!(!crashed.status.success(), "{step}");
        let listed = e.ok("a", &["work", "list"]);
        let items = listed["items"].as_array().unwrap();
        let migrated = items
            .iter()
            .find(|n| n["work_id"] == item.id)
            .unwrap_or_else(|| panic!("{step}: {listed}"));
        let path = migrated["path"].as_str().unwrap();
        assert!(path.starts_with(".wiki-island/work/"));
        assert_ne!(path, flat);
        assert_eq!(
            std::fs::read(e.base.join("a").join(path)).unwrap(),
            item.render().as_bytes()
        );
        assert!(!e.base.join("a").join(flat).exists());
        assert_eq!(migrated["actionable"], true);
        let again = e.ok("a", &["work", "list"]);
        assert_eq!(again["items"], listed["items"]);
    }
}

#[test]
fn sync_status_and_setup_do_not_contact_the_remote() {
    let e = Env::new();
    e.ok(
        "a",
        &[
            "sync",
            "setup",
            e.base.join("unavailable.git").to_str().unwrap(),
        ],
    );
    let status = e.ok("a", &["sync", "status"]);
    assert_eq!(status["configured"], true);
    assert_eq!(status["failures"], 0);
    let failed = e.run("a", &["sync"]);
    assert!(!failed.status.success());
    assert_eq!(e.ok("a", &["sync", "status"])["failures"], 1);
}

#[test]
fn an_editor_save_after_a_sync_crash_is_preserved_during_replay() {
    let e = Env::new();
    e.edit("a", "remote version\n");
    e.ok("a", &["write", "extra.md", "--content", "extra\n"]);
    e.ok("a", &["sync"]);
    let remote = std::fs::read(e.base.join("a/base.md")).unwrap();
    let crashed = e
        .command("b", &["sync"])
        .env("WI_FAULT", "sync_after_file")
        .output()
        .unwrap();
    assert!(!crashed.status.success());
    std::fs::write(e.base.join("b/base.md"), "editor version\n").unwrap();
    e.ok("b", &["read", "base.md"]);
    assert_eq!(std::fs::read(e.base.join("b/base.md")).unwrap(), remote);
    let copies: Vec<_> = std::fs::read_dir(e.base.join("b"))
        .unwrap()
        .flatten()
        .filter(|f| {
            f.file_name()
                .to_string_lossy()
                .contains(".wi-conflict-sync-")
        })
        .collect();
    assert_eq!(copies.len(), 1);
    assert_eq!(
        std::fs::read(copies[0].path()).unwrap(),
        b"editor version\n"
    );
    let requests = std::fs::read_to_string(e.base.join("b/.git/wi/work-requests.jsonl")).unwrap();
    assert!(requests.contains("editor version preserved"));
}

#[cfg(unix)]
#[test]
fn transport_runs_without_engine_locks_or_interactive_prompts() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    let bin = e.base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let wrapper = bin.join("git");
    let probe = std::env::current_exe()
        .unwrap()
        .display()
        .to_string()
        .replace('\'', "'\\''");
    std::fs::write(&wrapper, format!("#!/bin/sh\nWI_LOCK_PROBE_GITDIR=\"$2\" '{probe}' --exact transport_lock_probe_child --nocapture >/dev/null\nif [ \"$?\" -ne 0 ]; then exit 90; fi\nexec /usr/bin/git \"$@\"\n")).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = e
        .command("b", &["sync"])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(e.base.join("b/.git/wi/transport-lock-probe").is_file());
}

#[cfg(unix)]
#[test]
fn transport_timeout_kills_the_process_group_and_leaves_notes_available() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    let bin = e.base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let wrapper = bin.join("git");
    std::fs::write(&wrapper, "#!/bin/sh\nsleep 30 &\nwait\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let start = std::time::Instant::now();
    let output = e
        .command("b", &["sync"])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("WI_SYNC_TIMEOUT_MS", "50")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(start.elapsed().as_secs() < 5);
    e.ok("b", &["read", "base.md"]);
    assert_eq!(e.ok("b", &["sync", "status"])["failures"], 1);
}

#[test]
fn a_branch_moved_by_another_git_tool_does_not_leave_sync_stuck() {
    let e = Env::new();
    e.edit("a", "remote version\n");
    e.ok("a", &["write", "extra.md", "--content", "extra\n"]);
    e.ok("a", &["sync"]);
    let crashed = e
        .command("b", &["sync"])
        .env("WI_FAULT", "sync_after_file")
        .output()
        .unwrap();
    assert!(!crashed.status.success());
    std::fs::write(e.base.join("b/manual.md"), "manual history change\n").unwrap();
    e.git(&e.base.join("b"), &["add", "."]);
    e.git(
        &e.base.join("b"),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@localhost",
            "commit",
            "-m",
            "manual history change",
        ],
    );
    let head = e.git(&e.base.join("b"), &["rev-parse", "HEAD"]);
    e.ok("b", &["read", "manual.md"]);
    assert_eq!(e.git(&e.base.join("b"), &["rev-parse", "HEAD"]), head);
    assert!(!e.base.join("b/.git/wi/sync-intent.json").exists());
    assert!(
        !e.git(
            &e.base.join("b"),
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/wi-sync-recovery/"
            ]
        )
        .is_empty()
    );
    e.ok("b", &["sync"]);
    assert_eq!(
        std::fs::read(e.base.join("b/manual.md")).unwrap(),
        b"manual history change\n"
    );
    assert!(
        std::fs::read(e.base.join("b/extra.md"))
            .unwrap()
            .ends_with(b"extra\n")
    );
}

#[test]
fn transport_lock_probe_child() {
    let Some(root) = std::env::var_os("WI_LOCK_PROBE_GITDIR") else {
        return;
    };
    assert_eq!(std::env::var("GIT_TERMINAL_PROMPT").unwrap(), "0");
    assert_eq!(std::env::var("GCM_INTERACTIVE").unwrap(), "Never");
    assert!(
        std::env::var("GIT_SSH_COMMAND")
            .unwrap()
            .contains("-oBatchMode=yes")
    );
    let wi = PathBuf::from(root).join("wi");
    for name in ["publish.lock", "flush.lock"] {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(wi.join(name))
            .unwrap();
        file.try_lock().unwrap();
    }
    std::fs::write(wi.join("transport-lock-probe"), "checked").unwrap();
}

#[test]
fn large_file_exclusion_survives_a_crash_and_intervening_cli_reads() {
    for step in ["sync_after_exclude_request", "sync_after_exclude"] {
        let e = Env::new();
        std::fs::File::create(e.base.join("a/big.dat"))
            .unwrap()
            .set_len(50 * 1024 * 1024 + 1)
            .unwrap();
        let crashed = e
            .command("a", &["sync"])
            .env("WI_FAULT", step)
            .output()
            .unwrap();
        assert!(!crashed.status.success(), "{step}");
        e.ok("a", &["read", "base.md"]);
        e.ok("a", &["sync"]);
        assert!(
            !e.git(&e.base.join("a"), &["ls-tree", "-r", "--name-only", "HEAD"])
                .lines()
                .any(|p| p == "big.dat")
        );
        assert_eq!(
            std::fs::metadata(e.base.join("a/big.dat")).unwrap().len(),
            50 * 1024 * 1024 + 1
        );
        let requests =
            std::fs::read_to_string(e.base.join("a/.git/wi/work-requests.jsonl")).unwrap();
        assert_eq!(
            requests.lines().filter(|l| l.contains("big.dat")).count(),
            1
        );
    }
}
