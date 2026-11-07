//! CLI and crash tests for the write path. Each crash case aborts a
//! real `wi` process at a write-path step (`WI_FAULT`), then a later `wi`
//! process opens the vault and replays the manifest.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Env {
    _dir: TempDir,
    cache: TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        let env = Self {
            _dir: dir,
            cache,
            root,
        };
        let out = env.wi(&["init", env.root.to_str().unwrap()], None, None);
        assert!(out.status.success(), "{out:?}");
        env
    }

    fn wi(&self, args: &[&str], fault: Option<&str>, stdin: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wi"));
        cmd.args(args)
            .env("WI_CACHE_DIR", self.cache.path())
            .env("WI_VAULT", &self.root)
            .env_remove("WI_FAULT");
        if let Some(fault) = fault {
            cmd.env("WI_FAULT", fault);
        }
        match stdin {
            Some(input) => {
                use std::io::Write;
                cmd.stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let mut child = cmd.spawn().unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(input.as_bytes())
                    .unwrap();
                child.wait_with_output().unwrap()
            }
            None => cmd.output().unwrap(),
        }
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = args.to_vec();
        all.push("--json");
        let out = self.wi(&all, None, None);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn file(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).unwrap()
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.root, args)
    }

    fn manifest(&self) -> String {
        String::from_utf8_lossy(
            &std::fs::read(self.root.join(".git/wi/manifest.log")).unwrap_or_default(),
        )
        .into_owned()
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn checksum(env: &Env, rel: &str) -> String {
    env.json(&["read", rel])["checksum"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Write `old`, then replace it with `new` in a process aborted at `fault`.
fn crash_replace(env: &Env, rel: &str, old: &str, new: &str, fault: &str) {
    let out = env.wi(&["write", rel, "--content", old], None, None);
    assert!(out.status.success(), "{out:?}");
    let current = checksum(env, rel);
    let out = env.wi(
        &[
            "write",
            rel,
            "--content",
            new,
            "--expected-checksum",
            &current,
        ],
        Some(fault),
        None,
    );
    assert!(!out.status.success(), "the process must abort at {fault}");
}

#[test]
fn kill_before_intent_leaves_the_old_file_and_no_record() {
    let env = Env::new();
    crash_replace(
        &env,
        "a.md",
        "old text\n",
        "new text\n",
        "before_intent_fsync",
    );
    assert!(env.file("a.md").ends_with("old text\n"));
    assert!(
        !env.manifest().contains("\"kind\":\"intent\",\"seq\":3"),
        "{}",
        env.manifest()
    );
    let doctor = env.json(&["doctor"]);
    assert_eq!(doctor["checks"][1]["status"], "ok", "{doctor}");
    // The unacknowledged temp file is cleaned up by the next scan.
    let commit = env.json(&["commit"]);
    assert_eq!(
        commit["reconcile"]["removed_temps"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "{commit}"
    );
    assert_eq!(
        env.git(&["log", "--format=%s", "--", "a.md"])
            .lines()
            .count(),
        1
    );
}

#[test]
fn kill_after_intent_before_replace_drops_the_intent() {
    let env = Env::new();
    crash_replace(
        &env,
        "a.md",
        "old text\n",
        "new text\n",
        "after_intent_before_swap",
    );
    assert!(env.manifest().contains("\"kind\":\"intent\""));
    // The next process replays: the file has the old bytes, so the intent drops.
    let doctor = env.json(&["doctor"]);
    assert_eq!(
        doctor["checks"][1]["detail"], "no pending records",
        "{doctor}"
    );
    assert!(env.file("a.md").ends_with("old text\n"));
    assert_eq!(
        env.git(&["log", "--format=%s", "--", "a.md"])
            .lines()
            .count(),
        1
    );
    let leftovers: Vec<_> = std::fs::read_dir(&env.root)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".wi-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn kill_after_replace_before_done_completes_with_the_recorded_actor() {
    let env = Env::new();
    crash_replace(
        &env,
        "a.md",
        "old text\n",
        "new text\n",
        "after_swap_before_done",
    );
    assert!(env.file("a.md").ends_with("new text\n"));
    let read = env.json(&["read", "a.md"]);
    assert!(read["content"].as_str().unwrap().ends_with("new text\n"));
    let log = env.git(&["log", "--format=%B", "-1", "--", "a.md"]);
    assert!(log.contains("Wi-Actor: cli"), "{log}");
    assert_eq!(
        env.git(&["log", "--format=%s", "--", "a.md"])
            .lines()
            .count(),
        2
    );
    assert!(!log.contains("watcher"));
}

#[test]
fn third_version_after_a_crash_keeps_both_as_a_conflict_with_a_work_item() {
    let env = Env::new();
    crash_replace(
        &env,
        "a.md",
        "old text\n",
        "new text\n",
        "after_swap_before_done",
    );
    std::fs::write(env.root.join("a.md"), "third version\n").unwrap();
    let doctor = env.json(&["doctor"]);
    let work = doctor["work_requests"].as_array().unwrap();
    assert!(
        work.iter()
            .any(|w| w["kind"] == "recovery-conflict" && w["path"] == "a.md"),
        "{doctor}"
    );
    assert_eq!(env.file("a.md"), "third version\n");
    // The engine version is kept as a durable blob.
    let detail = work[0]["detail"].as_str().unwrap();
    let blob = detail.rsplit(' ').next().unwrap();
    let out = Command::new("git")
        .arg("-C")
        .arg(&env.root)
        .args(["cat-file", "-p", blob])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).ends_with("new text\n"));
}

#[test]
fn kill_after_done_keeps_the_save() {
    let env = Env::new();
    crash_replace(&env, "a.md", "old text\n", "new text\n", "after_done");
    assert!(env.file("a.md").ends_with("new text\n"));
    let search = env.json(&["search", "new"]);
    assert_eq!(search["hits"].as_array().unwrap().len(), 1, "{search}");
    env.json(&["commit"]);
    assert_eq!(
        env.git(&["log", "--format=%s", "--", "a.md"])
            .lines()
            .count(),
        2
    );
}

#[test]
fn index_failure_returns_pending_and_the_next_start_repairs_it() {
    let env = Env::new();
    let out = env.wi(
        &["write", "p.md", "--content", "pendingword\n", "--json"],
        Some("index_update"),
        None,
    );
    assert!(out.status.success(), "{out:?}");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["index"], "pending");
    assert!(env.root.join(".git/wi/index-pending").exists());
    let doctor = env.json(&["doctor"]);
    assert!(
        !env.root.join(".git/wi/index-pending").exists(),
        "repaired at open: {doctor}"
    );
    let search = env.json(&["search", "pendingword"]);
    assert_eq!(search["hits"].as_array().unwrap().len(), 1);
}

#[test]
fn offline_nested_edit_is_found_by_the_next_cli_query() {
    let env = Env::new();
    let out = env.wi(
        &["write", "a/b/c/deep.md", "--content", "first\n"],
        None,
        None,
    );
    assert!(out.status.success());
    env.json(&["search", "first"]);
    std::fs::write(
        env.root.join("a/b/c/deep.md"),
        "offlineword edited without a watcher\n",
    )
    .unwrap();
    let search = env.json(&["search", "offlineword"]);
    let hits = search["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{search}");
    assert_eq!(hits[0]["rel_path"], "a/b/c/deep.md");
    assert!(search["index_as_of"].is_string());
    assert!(search["generation"].is_u64());
}

#[test]
fn cli_refuses_protected_paths_with_a_typed_error() {
    let env = Env::new();
    for path in [".wiki-island/config.toml", ".git/HEAD.md", "../escape.md"] {
        let out = env.wi(&["write", path, "--content", "x", "--json"], None, None);
        assert!(!out.status.success());
        let body: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(body["error"]["code"], "protected-path", "{path}: {body}");
    }
}

#[test]
fn cli_end_to_end_init_write_search_commit_history_revert() {
    let env = Env::new();
    let first = env.wi(
        &["write", "notes/refund.md", "--stdin", "--json"],
        None,
        Some("---\ntitle: 환불 정책\n---\n결제 후 7일 안에 환불\n"),
    );
    assert!(first.status.success(), "{first:?}");
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["commits"], 1, "flushed at exit");
    let search = env.json(&["search", "환불"]);
    assert_eq!(search["hits"][0]["rel_path"], "notes/refund.md");
    let checksum = first["checksum"].as_str().unwrap();
    let edited = env.json(&[
        "edit",
        "notes/refund.md",
        "--set",
        "tags=[\"policy\"]",
        "--expected-checksum",
        checksum,
    ]);
    assert_ne!(edited["checksum"], first["checksum"]);
    env.json(&["commit"]);
    let history = env.json(&["history", "notes/refund.md"]);
    let entries = history.as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let oldest = entries[1]["commit"].as_str().unwrap();
    let reverted = env.json(&["revert", "notes/refund.md", oldest]);
    assert_eq!(reverted["checksum"], first["checksum"]);
    assert_eq!(
        env.json(&["history", "notes/refund.md"])
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let moved = env.json(&["move", "notes/refund.md", "policies/refund.md"]);
    assert_eq!(moved["moved_from"], "notes/refund.md");
    let archived = env.json(&["archive", "policies/refund.md"]);
    assert_eq!(archived["path"], "archive/policies/refund.md");
    let doctor = env.json(&["doctor"]);
    assert!(
        doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["status"] != "fail")
    );
    // Re-running init on an existing vault changes nothing.
    let again = env.json(&["init", env.root.to_str().unwrap()]);
    assert_eq!(again["commits"], 0);
}

#[test]
fn cli_reindex_after_index_deletion_returns_the_same_results() {
    let env = Env::new();
    for (i, word) in ["결제", "환불", "payment"].iter().enumerate() {
        let out = env.wi(
            &[
                "write",
                &format!("n{i}.md"),
                "--content",
                &format!("{word} note {i}\n"),
            ],
            None,
            None,
        );
        assert!(out.status.success());
    }
    let before = env.json(&["search", "note"]);
    for entry in std::fs::read_dir(env.cache.path()).unwrap().flatten() {
        std::fs::remove_dir_all(entry.path()).unwrap();
    }
    let reindex = env.json(&["reindex"]);
    assert_eq!(reindex["discovered"], 3);
    let after = env.json(&["search", "note"]);
    let paths = |v: &Value| {
        v["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["rel_path"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(paths(&before), paths(&after));
}

#[test]
fn recovery_never_touches_files_behind_a_changed_directory() {
    let env = Env::new();
    std::fs::create_dir(env.root.join("source")).unwrap();
    let created = env.json(&["write", "source/a.md", "--content=preserve me"]);
    let original = std::fs::read(env.root.join("source/a.md")).unwrap();
    let out = env.wi(
        &[
            "move",
            "source/a.md",
            "dest/a.md",
            "--expected-checksum",
            created["checksum"].as_str().unwrap(),
            "--op-id",
            "move-crash",
        ],
        Some("after_swap_before_done"),
        None,
    );
    assert!(!out.status.success(), "the process must abort");
    // Between calls, the source directory becomes a symlink to an outside
    // directory holding the same bytes.
    let outside = TempDir::new().unwrap();
    let victim = outside.path().join("a.md");
    std::fs::write(&victim, &original).unwrap();
    std::fs::rename(env.root.join("source"), env.root.join("old-source")).unwrap();
    std::os::unix::fs::symlink(outside.path(), env.root.join("source")).unwrap();
    let doctor = env.json(&["doctor"]);
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        original,
        "outside bytes kept"
    );
    let requests = doctor["work_requests"].as_array().unwrap();
    assert!(
        requests
            .iter()
            .any(|r| r["kind"] == "recovery-problem" && r["path"] == "source/a.md"),
        "{doctor:#}"
    );
    assert!(env.root.join("dest/a.md").exists());
}

#[test]
fn content_may_start_with_frontmatter_dashes_and_usage_errors_are_json() {
    let env = Env::new();
    let out = env.wi(
        &[
            "write",
            "good.md",
            "--content",
            "---\ntitle: Good\n---\nbody\n",
            "--json",
        ],
        None,
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(env.file("good.md").contains("title: Good"));
    let out = env.wi(&["write", "x.md", "--bogus", "--json"], None, None);
    assert_eq!(out.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["error"]["code"], "usage");
    // Engine-owned refusal still applies to hyphen-leading content.
    let out = env.wi(
        &[
            "write",
            "s.md",
            "--content",
            "---\nstatus: active\n---\n",
            "--json",
        ],
        None,
        None,
    );
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["error"]["code"], "engine-owned-field");
    assert!(body["error"]["hint"].is_string());
}

#[test]
fn cli_reindex_after_an_owner_deletion_finds_it_committed() {
    let env = Env::new();
    env.json(&["write", "gone.md", "--content=bye"]);
    std::fs::remove_file(env.root.join("gone.md")).unwrap();
    // The owner's deletion is committed as their action, with no work.
    env.json(&["reindex"]);
    let log = env.git(&["log", "--name-status", "--format=%s"]);
    assert!(log.contains("D\tgone.md"), "{log}");
    assert_eq!(env.git(&["status", "--porcelain"]).trim(), "");
    let doctor = env.json(&["doctor"]);
    assert!(
        !doctor["work_requests"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| r["path"] == "gone.md"),
        "{doctor}"
    );
}

#[test]
fn terminal_claim_edit_reports_typed_error_without_revival() {
    let env = Env::new();
    for status in ["superseded", "rejected"] {
        let path = format!("{status}.md");
        let original = format!(
            "---\ntype: memory\nstatus: {status}\nsummary: original\norigin: {{kind: user}}\n---\nbody\n"
        );
        std::fs::write(env.root.join(&path), &original).unwrap();
        let out = env.wi(
            &["--json", "edit", &path, "--set", "summary=changed"],
            None,
            None,
        );
        assert!(!out.status.success());
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(result["error"]["code"], "terminal-note", "{result}");
        assert_eq!(
            std::fs::read_to_string(env.root.join(&path)).unwrap(),
            original
        );
    }
}
