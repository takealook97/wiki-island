//! Lifecycle through the `wi` binary: a crash between the candidate and the
//! target write, the read modes of `wi search`, `wi sweep`, `wi work`
//! and `wi lint`.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};
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
        let out = env.wi(&["init", env.root.to_str().unwrap()], None);
        assert!(out.status.success(), "{out:?}");
        env
    }

    fn wi(&self, args: &[&str], fault: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wi"));
        cmd.args(args)
            .env("WI_CACHE_DIR", self.cache.path())
            .env("WI_VAULT", &self.root)
            .env_remove("WI_FAULT")
            .env_remove("WI_NOW");
        if let Some(fault) = fault {
            cmd.env("WI_FAULT", fault);
        }
        cmd.output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.wi(&all, None);
        assert!(out.status.success(), "{args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn status(&self, path: &str) -> Value {
        let bytes = std::fs::read(self.root.join(path)).unwrap();
        let note = wi_note::Note::parse(&bytes).unwrap();
        note.frontmatter()
            .and_then(|f| f.get("status"))
            .unwrap_or(Value::Null)
    }
}

fn claim(summary: &str, source: &[u8], value: u32, supersedes: &[&str]) -> String {
    let source_hash = wi_lifecycle::sha256(source);
    let len = source.len();
    format!(
        "---\ntype: memory\nkind: fact\nclaim_kind: historical\nsummary: {summary:?}\nentities: [{{\"id\": \"svc:web\", \"label\": null, \"source\": null}}]\nscope: work\nslot: port\nvalue: {value}\napplicability: {}\nobserved_at: 2026-01-0{}T00:00:00Z\nsources: [{{\"ref\": \"notes/runbook.md\", \"commit\": null, \"pending\": true, \"source_hash\": \"{source_hash}\", \"span_hash\": \"{source_hash}\", \"locator\": {{\"span\": {{\"start\": 0, \"end\": {len}}}}}}}]\n---\n{summary}.\n",
        json!({"from": null, "until": null, "conditions": [], "supersedes": supersedes}),
        if supersedes.is_empty() { 1 } else { 5 },
    )
}

#[test]
fn crash_after_the_candidate_write_leaves_the_target_not_current_and_sweep_recovers() {
    let env = Env::new();
    env.json(&[
        "write",
        "notes/runbook.md",
        "--content",
        "# Runbook\n\nport\n",
    ]);
    let bytes = std::fs::read(env.root.join("notes/runbook.md")).unwrap();
    let old = env.json(&[
        "write",
        "memory/a.md",
        "--content",
        &claim("Port 443", &bytes, 443, &[]),
    ]);
    assert_eq!(
        old["lifecycle"]["adoption"]["outcome"],
        json!("active"),
        "{old}"
    );
    // The process dies after the candidate became active, before the
    // target is marked superseded.
    let new = claim("Port 8443", &bytes, 8443, &["memory/a.md"]);
    let crashed = env.wi(
        &["write", "memory/b.md", "--content", &new],
        Some("lifecycle_after_candidate"),
    );
    assert!(!crashed.status.success());
    assert_eq!(env.status("memory/b.md"), json!("active"));
    assert_eq!(env.status("memory/a.md"), json!("active"));
    // Current-read rule: the target is already not current.
    let found = env.json(&["search", "Port"]);
    let paths: Vec<&str> = found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["rel_path"].as_str())
        .collect();
    assert!(paths.contains(&"memory/b.md"), "{found}");
    assert!(!paths.contains(&"memory/a.md"), "{found}");
    assert!(
        found["withheld"].to_string().contains("superseded"),
        "{found}"
    );
    let historic = env.json(&["search", "Port", "--read-mode", "historic"]);
    assert!(historic.to_string().contains("memory/a.md"), "{historic}");
    // The sweep's recovery pass completes the target status.
    let sweep = env.json(&["sweep"]);
    assert_eq!(sweep["recovered"], json!(["memory/a.md"]), "{sweep}");
    assert_eq!(env.status("memory/a.md"), json!("superseded"));
}

#[test]
fn cli_lint_work_and_capture_commands() {
    let env = Env::new();
    let note = "---\ntype: memory\nkind: fact\nclaim_kind: current-state\nsummary: Open\nentities: []\nscope: work\napplicability: {\"from\": null, \"until\": null, \"conditions\": [], \"supersedes\": []}\nsources: []\n---\nOpen.\n";
    let written = env.json(&["write", "memory/open.md", "--content", note]);
    assert_eq!(
        written["lifecycle"]["adoption"]["outcome"],
        json!("candidate")
    );
    let lint = env.json(&["lint", "memory/open.md"]);
    assert!(
        !lint["reports"][0]["findings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let work = env.json(&["work", "list"]);
    let gate = work["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["work_kind"] == json!("gate"))
        .expect("gate work item")["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let refused = env.wi(
        &["work", "close", &gate, "--outcome", "not_supported"],
        None,
    );
    assert!(!refused.status.success());
    env.json(&[
        "work",
        "close",
        &gate,
        "--outcome",
        "evidence_unavailable",
        "--limits",
        "no source yet",
        "--reopen-trigger",
        "source-change",
        "--reopen-detail",
        "a runbook is added",
    ]);
    let captured = env.json(&["capture", "the owner prefers short notes"]);
    assert_eq!(captured["capture_status"], json!("pending"));
    let dropped = env.wi(
        &[
            "capture",
            "token ghp_0123456789abcdefghijklmnopqrstuvwxyzAB",
        ],
        None,
    );
    assert!(!dropped.status.success());
}
