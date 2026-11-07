//! Public setup/skill surface, with every host and vault under a temporary home.
use serde_json::{Value, json};
use std::process::{Command, Output};
struct Env(tempfile::TempDir);
impl Env {
    fn new() -> Self {
        let env = Self(tempfile::tempdir().unwrap());
        std::fs::create_dir_all(env.0.path().join("home")).unwrap();
        assert!(env.run(&["init", "--json"]).status.success());
        env
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_wi"))
            .args(args)
            .env("HOME", self.0.path().join("home"))
            .env("CODEX_HOME", self.0.path().join("home/.codex"))
            .env("WI_VAULT", self.0.path().join("vault"))
            .env("WI_CACHE_DIR", self.0.path().join("cache"))
            .env("WI_LAUNCHCTL", "/usr/bin/false")
            .env_remove("WI_FAULT")
            .output()
            .unwrap()
    }
}
#[test]
fn help_hides_upkeep_commands_and_each_host_accepts_explicit_registration() {
    let env = Env::new();
    let help = env.run(&["--help"]);
    let text = String::from_utf8(help.stdout).unwrap();
    for visible in ["setup", "uninstall", "skills"] {
        assert!(
            text.lines()
                .any(|line| line.trim_start().starts_with(visible)),
            "{text}"
        );
    }
    for hidden in [
        "note", "reindex", "commit", "adopt", "sweep", "lint", "work",
    ] {
        assert!(
            !text
                .lines()
                .any(|line| line.trim_start().starts_with(&format!("{hidden} "))),
            "{text}"
        );
    }
    for host in [
        "claude",
        "codex",
        "agy",
        "gemini",
        "cursor",
        "windsurf",
        "claude-desktop",
        "vscode",
        "opencode",
        "zed",
    ] {
        let out = env.run(&["mcp", "install", "--host", host, "--yes", "--json"]);
        assert!(out.status.success(), "{host}: {out:?}");
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["applied"], true);
    }
    let config =
        std::fs::read_to_string(env.0.path().join("vault/.wiki-island/config.toml")).unwrap();
    assert!(config.contains("skill_targets"));
    assert!(config.contains("claude"));
    assert!(config.contains("agents"));
}
#[test]
fn skills_status_previews_sync_installs_and_all_read_modes_keep_working() {
    let env = Env::new();
    let fields = json!({"type":"memory","kind":"procedure","skill_id":"demo.note","title":"Read note","claim_kind":"intent","status":"active","summary":"Read the note.","entities":[],"scope":"personal","origin":{"kind":"user","locator":"file","captured_at":"2026-11-01T00:00:00Z"},"applicability":{"conditions":[],"supersedes":[]},"sources":[]});
    let note = format!(
        "---\n{}---\nRead the note.\n",
        fields
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| format!("{k}: {v}\n"))
            .collect::<String>()
    );
    std::fs::write(env.0.path().join("vault/procedure.md"), note).unwrap();
    let install = env.run(&["mcp", "install", "--host", "codex", "--yes", "--json"]);
    assert!(install.status.success(), "{install:?}");
    let hash = wi_lifecycle::sha256(b"demo.note");
    let file = env.0.path().join(format!(
        "home/.agents/skills/wi-read-note-{}/SKILL.md",
        &hash[7..15]
    ));
    assert!(file.is_file());
    std::fs::remove_file(&file).unwrap();
    let status = env.run(&["skills", "status", "--json"]);
    assert!(status.status.success(), "{status:?}");
    let report: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(report["updated"], 1);
    assert!(!file.exists());
    assert!(env.run(&["skills", "sync"]).status.success());
    assert!(file.is_file());
    for mode in ["current", "all", "historic", "explore"] {
        let out = env.run(&["search", "Read", "--read-mode", mode, "--json"]);
        assert!(out.status.success(), "{mode}: {out:?}");
    }
}

#[test]
fn help_frontmatter_and_import_errors_use_plain_actionable_text() {
    let env = Env::new();
    let help = String::from_utf8(env.run(&["--help"]).stdout).unwrap();
    assert!(help.contains("~/WikiIsland"));
    for internal in ["stat-walk", "sidecars", "manifest", "branch tip"] {
        assert!(!help.contains(internal), "{help}");
    }
    let error = env.run(&[
        "write",
        "new.md",
        "--content",
        "---\nsummary: Release: details\n---\nbody",
        "--json",
    ]);
    assert!(!error.status.success());
    let value: Value = serde_json::from_slice(&error.stdout).unwrap();
    assert_eq!(value["error"]["code"], "frontmatter-invalid", "{value}");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("line 2")
    );
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("quote")
    );
    let failed = env.run(&["import", "missing.zip"]);
    assert!(!failed.status.success());
    let text = String::from_utf8(failed.stdout).unwrap();
    assert!(text.contains("failed:"), "{text}");
    assert!(
        text.contains("No such file") || text.contains("not found"),
        "{text}"
    );
}

#[test]
fn doctor_and_maintenance_count_ready_tasks() {
    let env = Env::new();
    std::fs::write(
        env.0.path().join("vault/conflicted.md"),
        "<<<<<<< ours\nleft\n=======\nright\n>>>>>>> theirs\n",
    )
    .unwrap();
    let maintained = env.run(&["maintain", "--json"]);
    assert!(maintained.status.success(), "{maintained:?}");
    let maintained: Value = serde_json::from_slice(&maintained.stdout).unwrap();
    let ready = maintained["tasks_ready"].as_u64().unwrap();
    assert!(ready > 0, "{maintained}");
    let doctor = env.run(&["doctor", "--json"]);
    assert!(doctor.status.success(), "{doctor:?}");
    let doctor: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let work = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "work")
        .unwrap();
    assert_eq!(work["detail"], format!("{ready} task(s) for your AI"));
    let text = env.run(&["maintain"]);
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("task(s) for your AI"), "{text}");
    assert!(!text.contains("Everything is in order"), "{text}");
}

#[test]
fn import_failures_explain_missing_and_wrong_exports_in_plain_language() {
    let env = Env::new();
    let missing = env.run(&["import", "missing.zip"]);
    assert!(!missing.status.success());
    let text = String::from_utf8(missing.stdout).unwrap();
    assert!(text.contains("source file was not found"), "{text}");
    assert!(!text.contains("os error"), "{text}");
    std::fs::create_dir(env.0.path().join("vault/export")).unwrap();
    std::fs::write(env.0.path().join("vault/export/plain.md"), "# Plain\n").unwrap();
    // A zip without any page identifiers is a document-format failure.
    let mut empty_zip = vec![0; 22];
    empty_zip[..4].copy_from_slice(b"PK\x05\x06");
    std::fs::write(env.0.path().join("vault/empty.zip"), empty_zip).unwrap();
    let wrong = env.run(&["import", "empty.zip"]);
    assert!(!wrong.status.success());
    let text = String::from_utf8(wrong.stdout).unwrap();
    assert!(text.contains("Export pages as Markdown and CSV"), "{text}");
    assert!(!text.contains("invalid document request"), "{text}");
}
