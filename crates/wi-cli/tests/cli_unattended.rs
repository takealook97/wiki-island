//! Unattended CLI output contracts; all filesystem state stays under a temporary home.
use serde_json::Value;
use std::process::{Command, Output};

struct Env(tempfile::TempDir);
impl Env {
    fn new() -> Self {
        let env = Self(tempfile::tempdir().unwrap());
        std::fs::create_dir_all(env.0.path().join("home")).unwrap();
        let out = env.run(&["init", "--json"]);
        assert!(out.status.success(), "{out:?}");
        env
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wi"));
        cmd.env("HOME", self.0.path().join("home"))
            .env("USERPROFILE", self.0.path().join("home"))
            .env("APPDATA", self.0.path().join("home"))
            .env("LOCALAPPDATA", self.0.path().join("home"))
            .env("CODEX_HOME", self.0.path().join("home/.codex"))
            .env("WI_VAULT", self.0.path().join("vault"))
            .env("WI_CACHE_DIR", self.0.path().join("cache"))
            .env_remove("WI_FAULT");
        cmd
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.0.path().join("vault").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}
fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn search_reports_ingest_failures_in_json_and_keeps_human_warning() {
    let env = Env::new();
    let note = "---\ntype: memory\nstatus: rejected\norigin:\n  kind: user\n---\nold body\n";
    env.write("failed.md", note);
    assert!(env.run(&["commit"]).status.success());
    env.write("failed.md", note.replace("old body", "changed body"));
    env.write("good.md", "searchable zebra\n");
    let out = env.run(&["search", "zebra", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let value = json(&out);
    let warnings = value["warnings"].as_array().expect("top-level warnings");
    assert!(
        warnings.iter().any(|w| w["path"] == "failed.md"
            && w["code"] == "ingest-failed"
            && w["message"].as_str().is_some_and(|m| !m.is_empty())),
        "{value}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("failed.md is not in history yet"));
    assert!(String::from_utf8_lossy(&out.stdout).contains("good.md"));
}

#[test]
fn import_expands_nested_directories_skips_hidden_and_reports_failure_codes() {
    let env = Env::new();
    env.write("docs/a.csv", "a,b\n1,2\n");
    env.write("docs/nested/b.CSV", "c,d\n3,4\n");
    env.write("docs/.hidden.csv", "hidden\n");
    env.write("docs/.git/secret.csv", "hidden\n");
    env.write("docs/nested/readme.md", "not a source\n");
    let out = env.run(&["import", "docs", "--ocr", "off", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let value = json(&out);
    let reports = value["imports"].as_array().unwrap();
    assert_eq!(reports.len(), 2, "{value}");
    assert!(reports.iter().all(|r| r["status"] == "complete"));
    let out = env.run(&["import", "docs/nested/readme.md", "missing.csv", "--json"]);
    assert!(!out.status.success());
    assert_eq!(json(&out)["imports"][0]["code"], "document-invalid");
    assert_eq!(json(&out)["imports"][1]["code"], "document-io");
}

#[test]
fn host_install_errors_have_a_stable_code() {
    let env = Env::new();
    std::fs::write(env.0.path().join("home/.claude.json"), "{invalid").unwrap();
    let out = env.run(&["mcp", "install", "--host", "claude", "--json"]);
    assert!(!out.status.success());
    assert_eq!(json(&out)["error"]["code"], "install-config");
}

#[test]
fn watch_stdout_is_ndjson_including_events_and_final_summary() {
    let env = Env::new();
    let mut child = env
        .command()
        .args(["watch", "--duration", "2", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::BufRead;
    let mut ready = String::new();
    let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    stderr.read_line(&mut ready).unwrap();
    child.stderr = Some(stderr.into_inner());
    assert!(ready.starts_with("watching "), "{ready}");
    std::thread::sleep(std::time::Duration::from_millis(200));
    env.write("watched.md", "watch event\n");
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("each stdout line is one JSON value"))
        .collect();
    assert!(
        lines.len() > 1,
        "expected watcher event and summary: {text}"
    );
    assert!(lines.last().unwrap()["events"].as_u64().unwrap() > 0);
    assert!(lines.last().unwrap()["commits"].is_number());
}

#[cfg(target_os = "macos")]
#[test]
fn cli_refuses_a_dev_service_binary_without_invoking_launchctl() {
    let env = Env::new();
    let binary = env.0.path().join("target/debug/wi");
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_wi"), &binary).unwrap();
    std::fs::write(
        env.0.path().join("target/CACHEDIR.TAG"),
        "cargo target cache",
    )
    .unwrap();
    let out = Command::new(binary)
        .args(["service", "install", "--json"])
        .env("HOME", env.0.path().join("home"))
        .env("USERPROFILE", env.0.path().join("home"))
        .env("APPDATA", env.0.path().join("home"))
        .env("LOCALAPPDATA", env.0.path().join("home"))
        .env("WI_VAULT", env.0.path().join("vault"))
        .env("WI_CACHE_DIR", env.0.path().join("cache"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(json(&out)["error"]["code"], "install-config");
    assert!(
        json(&out)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cargo install --path crates/wi-cli")
    );
    assert!(!env.0.path().join("home/Library").exists());
}

#[test]
fn history_and_error_responses_keep_ingest_warnings_machine_visible() {
    let env = Env::new();
    let note = "---\ntype: memory\nstatus: rejected\norigin:\n  kind: user\n---\nold body\n";
    env.write("failed.md", note);
    env.write("history.md", "tracked note\n");
    assert!(env.run(&["commit"]).status.success());
    env.write("failed.md", note.replace("old body", "changed body"));
    let history = env.run(&["history", "history.md", "--json"]);
    assert!(history.status.success(), "{history:?}");
    assert!(json(&history).is_array());
    let warnings: Value = serde_json::from_slice(&history.stderr).unwrap();
    assert_eq!(warnings["warnings"][0]["path"], "failed.md");
    assert_eq!(String::from_utf8_lossy(&history.stderr).lines().count(), 1);
    env.write(
        "failed.md",
        note.replace("old body", "changed for human body"),
    );
    let human = env.run(&["history", "history.md"]);
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stderr).contains("failed.md is not in history yet"));
    assert!(!String::from_utf8_lossy(&human.stdout).contains("warning:"));
    env.write("failed.md", note.replace("old body", "changed again body"));
    let error = env.run(&["read", "missing.md", "--json"]);
    assert!(!error.status.success());
    assert_eq!(json(&error)["error"]["code"], "not-found");
    assert_eq!(json(&error)["warnings"][0]["path"], "failed.md");
}

#[cfg(unix)]
#[test]
fn directory_import_never_follows_a_symlink_outside_the_vault() {
    let env = Env::new();
    env.write("docs/a.csv", "a,b\n");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("outside.csv"), "private\n").unwrap();
    std::os::unix::fs::symlink(
        outside.path(),
        env.0.path().join("vault").join("docs").join("link"),
    )
    .unwrap();
    let out = env.run(&["import", "docs", "--ocr", "off", "--json"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(json(&out)["imports"].as_array().unwrap().len(), 1);
    assert!(!outside.path().join("outside.csv.md").exists());
}

#[test]
fn extraction_failure_reports_have_a_stable_code() {
    let env = Env::new();
    env.write("broken.docx", "not a ZIP document");
    let out = env.run(&["import", "broken.docx", "--ocr", "off", "--json"]);
    assert!(!out.status.success());
    let report = json(&out);
    assert_eq!(report["imports"][0]["status"], "failed");
    assert_eq!(report["imports"][0]["code"], "document-extraction");
}

#[test]
fn directory_import_skips_ignore_rules_but_explicit_files_report_failure() {
    let env = Env::new();
    env.write(".gitignore", "ignored/\n");
    env.write(".ignore", "omitted.csv\n");
    env.write("ignored/x.csv", "a,b\n");
    env.write("omitted.csv", "a,b\n");
    env.write("ok.csv", "a,b\n");
    let out = env.run(&["import", ".", "--ocr", "off", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let value = json(&out);
    assert_eq!(value["imports"].as_array().unwrap().len(), 1, "{value}");
    assert_eq!(value["imports"][0]["status"], "complete");
    let explicit = env.run(&["import", "ignored/x.csv", "--ocr", "off", "--json"]);
    assert!(!explicit.status.success());
    assert_eq!(json(&explicit)["imports"][0]["status"], "failed");
}

#[cfg(unix)]
#[test]
fn directory_import_reports_unreadable_subdirectory_and_imports_siblings() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    env.write("docs/ok.csv", "a,b\n");
    env.write("docs/locked/x.csv", "c,d\n");
    let locked = env.0.path().join("vault/docs/locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let unreadable = std::fs::read_dir(&locked).is_err();
    let out = env.run(&["import", "docs", "--ocr", "off", "--json"]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if !unreadable {
        return;
    } // A privileged runner can bypass chmod permissions.
    assert!(!out.status.success());
    let value = json(&out);
    let reports = value["imports"].as_array().unwrap();
    assert!(
        reports
            .iter()
            .any(|r| r["path"] == "docs/locked" && r["code"] == "document-io")
    );
    assert!(reports.iter().any(|r| r["status"] == "complete"));
}

#[test]
fn csv_import_retries_after_budget_exhaustion() {
    let env = Env::new();
    env.write("data.csv", "a,b\n1,2\n");
    let first = env
        .command()
        .args(["import", "data.csv", "--ocr", "off", "--json"])
        .env("WI_IMPORT_BUDGET_MS", "0")
        .output()
        .unwrap();
    assert_eq!(json(&first)["imports"][0]["status"], "partial");
    let second = env.run(&["import", "data.csv", "--ocr", "off", "--json"]);
    assert!(second.status.success(), "{second:?}");
    let report = json(&second);
    assert_eq!(report["imports"][0]["status"], "complete", "{report}");
    assert_eq!(report["imports"][0]["unchanged"], false);
    assert_eq!(
        report["imports"][0]["run_note_path"],
        json(&first)["imports"][0]["run_note_path"]
    );
    let third = env.run(&["import", "data.csv", "--ocr", "off", "--json"]);
    assert_eq!(json(&third)["imports"][0]["unchanged"], true);
}

#[cfg(unix)]
#[test]
fn image_import_retries_after_tool_timeout() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let bin = env.0.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let tool = bin.join("tesseract");
    let script = "#!/bin/sh\ncase \"$1\" in\n--version) echo fake-tesseract; exit 0;;\n--list-langs) echo eng; exit 0;;\nesac\nif [ -n \"$SLOW_OCR\" ]; then /bin/sleep 30; fi\nprintf 'level\\tpage_num\\tblock_num\\tpar_num\\tline_num\\tword_num\\tleft\\ttop\\twidth\\theight\\tconf\\ttext\\n5\\t1\\t1\\t1\\t1\\t1\\t0\\t0\\t1\\t1\\t99\\thello\\n'\n";
    std::fs::write(&tool, script).unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    env.write(
        "img.png",
        [
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 0, 2,
            0, 0, 5, 0, 1, 165, 246, 69, 64, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ],
    );
    let first = env
        .command()
        .args(["import", "img.png", "--json"])
        .env("PATH", &bin)
        .env("WI_TOOL_TIMEOUT_MS", "3000")
        .env("SLOW_OCR", "1")
        .output()
        .unwrap();
    let first_report = json(&first);
    assert_eq!(
        first_report["imports"][0]["status"], "needs_ocr",
        "{first_report}"
    );
    assert!(
        first_report["imports"][0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().unwrap().contains("tool-timeout"))
    );
    let second = env
        .command()
        .args(["import", "img.png", "--json"])
        .env("PATH", &bin)
        .env_remove("WI_TOOL_TIMEOUT_MS")
        .env_remove("SLOW_OCR")
        .output()
        .unwrap();
    assert!(second.status.success(), "{second:?}");
    let report = json(&second);
    assert_eq!(report["imports"][0]["status"], "complete", "{report}");
    assert_eq!(report["imports"][0]["unchanged"], false);
}

#[cfg(unix)]
#[test]
fn directory_import_reports_non_utf8_entry_and_imports_siblings() {
    use std::os::unix::ffi::OsStringExt;
    let env = Env::new();
    env.write("docs/ok.csv", "a,b\n");
    let invalid = std::ffi::OsString::from_vec(b"invalid-\xff.csv".to_vec());
    let path = env.0.path().join("vault/docs").join(invalid);
    if let Err(error) = std::fs::write(path, "a,b\n") {
        eprintln!("filesystem does not support non-UTF-8 filenames: {error}");
        return;
    }
    let out = env.run(&["import", "docs", "--ocr", "off", "--json"]);
    assert!(!out.status.success());
    let value = json(&out);
    let reports = value["imports"].as_array().unwrap();
    assert!(
        reports.iter().any(|r| r["code"] == "document-invalid"
            && r["path"].as_str().unwrap().starts_with("docs/invalid-")),
        "{value}"
    );
    assert!(reports.iter().any(|r| r["status"] == "complete"), "{value}");
}

#[cfg(windows)]
#[test]
fn directory_import_never_follows_a_junction_outside_the_vault() {
    let env = Env::new();
    env.write("docs/a.csv", "a,b\n");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("outside.csv"), "private\n").unwrap();
    let output = Command::new("cmd")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(env.0.path().join("vault").join("docs").join("link"))
        .arg(outside.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = env.run(&["import", "docs", "--ocr", "off", "--json"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(json(&output)["imports"].as_array().unwrap().len(), 1);
    assert!(!outside.path().join("outside.csv.md").exists());
}
