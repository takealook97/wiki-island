//! Zero-care wiring: one-shot commands ingest editor changes and commit before returning,
//! and `wi service` previews without touching the system.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Env {
    _dir: TempDir,
    cache: TempDir,
    home: TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        let env = Self {
            _dir: dir,
            cache: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
            root,
        };
        let out = env.wi(&["init", env.root.to_str().unwrap()]);
        assert!(out.status.success(), "{out:?}");
        env
    }

    fn wi(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_wi"))
            .args(args)
            .env("WI_CACHE_DIR", self.cache.path())
            .env("WI_VAULT", &self.root)
            .env("HOME", self.home.path())
            .env_remove("WI_FAULT")
            .output()
            .unwrap()
    }

    /// A note saved by an editor: bytes on disk, no `wi` involved.
    fn editor_note(&self, name: &str) {
        std::fs::write(
            self.root.join(name),
            format!("# {name}\n\nwritten by an editor, mentions zebracrossing\n"),
        )
        .unwrap();
    }

    fn git_log(&self) -> String {
        let out = Command::new("git")
            .args(["log", "--name-only", "--format=%s"])
            .current_dir(&self.root)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

fn assert_ok(out: &Output) {
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn search_commits_an_editor_note_before_returning() {
    let env = Env::new();
    env.editor_note("editor-search.md");
    let out = env.wi(&["search", "zebracrossing", "--json"]);
    assert_ok(&out);
    assert!(String::from_utf8_lossy(&out.stdout).contains("editor-search.md"));
    assert!(env.git_log().contains("editor-search.md"));
}

#[test]
fn read_commits_an_editor_note_before_returning() {
    let env = Env::new();
    env.editor_note("editor-read.md");
    assert_ok(&env.wi(&["read", "editor-read.md"]));
    assert!(env.git_log().contains("editor-read.md"));
}

#[test]
fn history_shows_the_editor_note_it_just_committed() {
    let env = Env::new();
    env.editor_note("editor-history.md");
    let out = env.wi(&["history", "editor-history.md"]);
    assert_ok(&out);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("no history"));
    assert!(env.git_log().contains("editor-history.md"));
}

#[test]
fn doctor_commits_an_editor_note_and_reports_nothing_unseen() {
    let env = Env::new();
    env.editor_note("editor-doctor.md");
    let out = env.wi(&["doctor", "--json"]);
    assert_ok(&out);
    assert!(env.git_log().contains("editor-doctor.md"));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let unseen = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "unseen")
        .expect("doctor has an unseen check");
    assert_eq!(unseen["status"], "ok", "{unseen}");
}

#[test]
fn service_install_without_yes_previews_and_writes_nothing() {
    let env = Env::new();
    let out = env.wi(&["service", "install", "--allow-dev-binary"]);
    assert_ok(&out);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("Preview only; pass --yes to apply."),
        "{text}"
    );
    assert!(text.contains("<plist"), "{text}");
    assert!(
        std::fs::read_dir(env.home.path()).unwrap().next().is_none(),
        "install preview must not write under HOME"
    );
}

// `wi service status` and `uninstall` always reach `/bin/launchctl`, so no CLI test runs them;
// their logic is covered with a fake launchctl in `wi-export/tests/service.rs`.

fn work_requests(env: &Env) -> String {
    std::fs::read_to_string(env.root.join(".git/wi/work-requests.jsonl")).unwrap_or_default()
}

#[cfg(unix)]
fn unseen_check(out: &Output) -> Value {
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "unseen")
        .expect("doctor has an unseen check")
        .clone()
}

#[test]
fn a_failing_path_does_not_keep_later_editor_notes_out_of_history() {
    let env = Env::new();
    let note = "---\ntype: memory\nstatus: rejected\norigin:\n  kind: user\n---\nold body\n";
    std::fs::write(env.root.join("a.md"), note).unwrap();
    assert_ok(&env.wi(&["commit"]));
    std::fs::write(env.root.join("a.md"), note.replace("old body", "new body")).unwrap();
    std::fs::write(env.root.join("z.md"), "new late editor file\n").unwrap();
    let out = env.wi(&["search", "editor", "--json"]);
    assert_ok(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("a.md is not in history yet"), "{stderr}");
    assert!(env.git_log().contains("z.md"), "{}", env.git_log());
    let requests = work_requests(&env);
    assert!(
        requests
            .lines()
            .any(|l| l.contains("\"ingest-failed\"") && l.contains("\"a.md\"")),
        "{requests}"
    );
}

#[test]
fn import_also_commits_an_unrelated_editor_note() {
    let env = Env::new();
    env.editor_note("editor.md");
    std::fs::write(env.root.join("a.csv"), "a,b\n1,2\n").unwrap();
    let out = env.wi(&["import", "a.csv", "--ocr", "off", "--json"]);
    assert_ok(&out);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        report["commits"], 1,
        "the import is still one bulk commit: {report}"
    );
    let log = env.git_log();
    assert!(log.contains("editor.md"), "{log}");
    assert!(log.contains("a.csv"), "{log}");
}

#[test]
fn deleting_a_just_written_note_is_committed_without_work() {
    let env = Env::new();
    assert_ok(&env.wi(&["write", "written.md", "--content", "new text"]));
    std::fs::remove_file(env.root.join("written.md")).unwrap();
    assert_ok(&env.wi(&["doctor", "--json"]));
    let requests = work_requests(&env);
    assert!(!requests.contains("\"written.md\""), "{requests}");
    let out = Command::new("git")
        .args(["log", "--name-status", "--format=%s"])
        .current_dir(&env.root)
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(log.contains("D\twritten.md"), "{log}");
}

/// Restores a file's mode when the test ends, pass or fail.
#[cfg(unix)]
struct RestoreMode(PathBuf);

#[cfg(unix)]
impl Drop for RestoreMode {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o644));
    }
}

#[cfg(unix)]
#[test]
fn doctor_warns_about_a_file_it_cannot_read() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let path = env.root.join("locked.md");
    std::fs::write(&path, "owner text\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _restore = RestoreMode(path.clone());
    if std::fs::read(&path).is_ok() {
        return; // running as root: permissions do not apply
    }
    let out = env.wi(&["doctor", "--json"]);
    assert_ok(&out);
    let unseen = unseen_check(&out);
    assert_eq!(unseen["status"], "warn", "{unseen}");
    assert!(
        unseen["detail"].as_str().unwrap().contains("locked.md"),
        "{unseen}"
    );
    assert!(
        work_requests(&env).contains("\"ingest-failed\""),
        "{}",
        work_requests(&env)
    );
}

#[test]
fn maintain_tidies_once_then_finds_nothing_and_refreshes_skills() {
    let env = Env::new();
    let config = env.root.join(".wiki-island/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("skill_targets = [\"claude\"]\n{text}")).unwrap();
    wi_export::local::Targets {
        home: env.home.path().to_owned(),
        hosts: vec![],
        skill_dirs: vec![env.home.path().join(".claude/skills")],
        cache_root: env.cache.path().to_owned(),
    }
    .save(&env.root)
    .unwrap();
    let out = env.wi(&["maintain", "--json"]);
    assert_ok(&out);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["procedures_changed"], true, "{report}");
    assert_eq!(report["complete"], true, "{report}");
    // The built-in maintenance procedure reached the host skills folder.
    let skills = env.home.path().join(".claude/skills");
    let copies = std::fs::read_dir(&skills)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("SKILL.md").is_file())
        .count();
    assert!(copies >= 1, "no skill copies under {}", skills.display());
    let log = env.git_log();
    let out = env.wi(&["maintain", "--full"]);
    assert_ok(&out);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "Everything is in order."
    );
    assert_eq!(env.git_log(), log, "a quiet pass commits nothing");
}

#[test]
fn doctor_reports_materialized_agent_tasks_without_stale_request_warnings() {
    let env = Env::new();
    std::fs::write(
        env.root.join("conflicted.md"),
        "<<<<<<< ours\nleft\n=======\nright\n>>>>>>> theirs\n",
    )
    .unwrap();
    assert_ok(&env.wi(&["maintain"]));
    let out = env.wi(&["doctor", "--json"]);
    assert_ok(&out);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let work = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "work")
        .unwrap();
    assert_eq!(work["status"], "ok", "{report}");
    assert!(
        work["detail"]
            .as_str()
            .unwrap()
            .contains("task(s) for your AI")
    );
    assert!(!report["work_requests"].as_array().unwrap().is_empty());
}
