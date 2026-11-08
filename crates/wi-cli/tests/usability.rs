//! CLI usability: search snippets, piped capture, watch start-up, init hint.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

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
        let root = dir.path().join("vault");
        let env = Self {
            _dir: dir,
            cache: TempDir::new().unwrap(),
            root,
        };
        let out = env.wi(&["init", env.root.to_str().unwrap()], None);
        assert!(out.status.success(), "{out:?}");
        env
    }

    fn wi(&self, args: &[&str], stdin: Option<&str>) -> Output {
        use std::io::Write;
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wi"));
        cmd.args(args)
            .env("WI_CACHE_DIR", self.cache.path())
            .env("WI_NOW", "2026-01-01T00:00:00Z")
            .env("WI_VAULT", &self.root)
            .env_remove("WI_FAULT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut pipe = child.stdin.take().unwrap();
        if let Some(input) = stdin {
            pipe.write_all(input.as_bytes()).unwrap();
        }
        drop(pipe);
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.wi(args, None);
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
}

fn git_log(root: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(["-c", "core.autocrlf=false"])
        .arg("-C")
        .arg(root)
        .args(["log", "--name-only", "--format=%s"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn search_shows_a_one_line_snippet_under_each_hit() {
    let env = Env::new();
    env.ok(&[
        "write",
        "notes/a.md",
        "--content",
        "# Title\n\nfirst line about zebracrossing\nsecond line\n",
    ]);
    let text = env.ok(&["search", "zebracrossing"]);
    let snippet_line = text
        .lines()
        .find(|l| l.starts_with("    ") && l.contains("zebracrossing"))
        .unwrap_or_else(|| panic!("no snippet line in {text}"));
    assert!(!snippet_line.trim().is_empty());
}

#[test]
fn capture_from_stdin_matches_capture_from_argument() {
    let by_arg = Env::new();
    let by_stdin = Env::new();
    let a: Value =
        serde_json::from_str(&by_arg.ok(&["capture", "remember the milk", "--json"])).unwrap();
    let out = by_stdin.wi(&["capture", "--stdin", "--json"], Some("remember the milk"));
    assert!(out.status.success(), "{out:?}");
    let b: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(a["capture_status"], b["capture_status"], "{a} {b}");
    let path = a["capture"].as_str().unwrap();
    assert_eq!(path, b["capture"].as_str().unwrap());
    let normalize_time = |root: &std::path::Path| {
        let bytes = std::fs::read(root.join(path)).unwrap();
        let note = wi_note::Note::parse(&bytes).unwrap();
        let mut origin = note.frontmatter().unwrap().get("origin").unwrap();
        assert!(wi_lifecycle::parse_time(origin["captured_at"].as_str().unwrap()).is_ok());
        // Separate saves can cross a second boundary; compare every other byte.
        origin["captured_at"] = serde_json::json!("2000-01-01T00:00:00Z");
        wi_note::set_key(&note, "origin", &origin)
            .unwrap()
            .to_bytes()
            .to_vec()
    };
    assert_eq!(normalize_time(&by_arg.root), normalize_time(&by_stdin.root));
}

#[test]
fn capture_from_file_works_and_two_inputs_are_a_usage_error() {
    let env = Env::new();
    let file = env._dir.path().join("in.txt");
    std::fs::write(&file, "from a file").unwrap();
    env.ok(&["capture", "--file", file.to_str().unwrap()]);
    let both = env.wi(&["capture", "text", "--stdin"], Some("x"));
    assert_eq!(both.status.code(), Some(2), "{both:?}");
    let none = env.wi(&["capture"], None);
    assert_eq!(none.status.code(), Some(2), "{none:?}");
}

#[test]
fn watch_reconciles_files_created_while_it_was_not_running() {
    let env = Env::new();
    std::fs::create_dir_all(env.root.join("notes")).unwrap();
    std::fs::write(env.root.join("notes/offline.md"), "# Offline\n\nbody\n").unwrap();
    let out = env.wi(&["watch", "--duration", "3"], None);
    assert!(out.status.success(), "{out:?}");
    assert!(
        git_log(&env.root).contains("notes/offline.md"),
        "{}",
        git_log(&env.root)
    );
}

#[test]
fn init_prints_the_service_next_step() {
    let dir = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_wi"))
        .args(["init"])
        .arg(dir.path().join("v"))
        .env("WI_CACHE_DIR", cache.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("next: wi service install --yes  (keeps this vault committed and indexed in the background)"),
        "{text}"
    );
}
