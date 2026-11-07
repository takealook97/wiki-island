//! Existing vault adoption, edits, moves and context workflows.
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;
use wi_note::Note;

const TOPIC: &str = include_str!("fixtures/workflows/topic.md");
const TARGET: &str = include_str!("fixtures/workflows/target.md");

struct Project {
    _temp: TempDir,
    root: PathBuf,
    cache: PathBuf,
}

impl Project {
    fn existing() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("project");
        let cache = temp.path().join("cache");
        std::fs::create_dir_all(root.join("policies")).unwrap();
        std::fs::create_dir_all(root.join("notes")).unwrap();
        std::fs::write(root.join("policies/refunds.md"), TOPIC).unwrap();
        std::fs::write(root.join("notes/target.md"), TARGET).unwrap();
        Self {
            _temp: temp,
            root,
            cache,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_wi"))
            .args(args)
            .arg("--json")
            .env("WI_VAULT", &self.root)
            .env("WI_CACHE_DIR", &self.cache)
            .env_remove("WI_FAULT")
            .env_remove("WI_NOW")
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn init(&self) {
        self.json(&["init", self.root.to_str().unwrap()]);
    }

    fn bytes(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.root.join(path)).unwrap()
    }

    fn checksum(&self, path: &str) -> String {
        self.json(&["read", path])["checksum"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

#[test]
fn open_in_place_preserves_authored_bytes_and_plain_legacy_status() {
    let p = Project::existing();
    // Adoption preserves unrelated attachments and ignore rules too.
    std::fs::write(p.root.join("attachment.bin"), b"\0\xff\x01").unwrap();
    std::fs::write(p.root.join(".gitignore"), "authored-rule\n").unwrap();
    p.init();
    assert_eq!(p.bytes("policies/refunds.md"), TOPIC.as_bytes());
    assert_eq!(p.bytes("notes/target.md"), TARGET.as_bytes());
    assert_eq!(p.bytes("attachment.bin"), b"\0\xff\x01");
    assert!(p.root.join(".wiki-island/config.toml").is_file());
    assert!(p.root.join(".git").is_dir());
    assert!(
        String::from_utf8(p.bytes(".gitignore"))
            .unwrap()
            .starts_with("authored-rule\n")
    );
    for query in ["refundtoken", "환불 정책", "\"Refund Policy\""] {
        let found = p.json(&["search", query]);
        assert_eq!(
            found["hits"][0]["rel_path"], "policies/refunds.md",
            "{found}"
        );
        assert_eq!(found["lifecycle"][0]["lifecycle"], "none");
        assert_eq!(found["lifecycle"][0]["eligibility"], "plain");
        assert_eq!(found["hits"][0]["metadata"]["status"], "active");
    }
    let again = p.json(&["init", p.root.to_str().unwrap()]);
    assert_eq!(again["created_config"], false);
    assert_eq!(p.bytes("policies/refunds.md"), TOPIC.as_bytes());
}

#[test]
fn edit_move_archive_keep_identity_relations_and_unknown_metadata() {
    let p = Project::existing();
    p.init();
    let before = p.checksum("policies/refunds.md");
    p.json(&[
        "edit",
        "policies/refunds.md",
        "--append",
        "\nRoundtrip addition.\n",
        "--set",
        "new_string=\"yes\"",
        "--expected-checksum",
        &before,
    ]);
    let edited = p.bytes("policies/refunds.md");
    let edited_text = std::str::from_utf8(&edited).unwrap();
    for untouched in [
        "custom_key: \"Keep: exactly\" # authored comment\n",
        "unknown_list: [z, a]\n",
        "quoted_yes: \"yes\"\n",
        "quoted_date: \"2026-11-05\"\n",
        "- relates_to [[notes/target]]\n",
    ] {
        assert!(
            edited_text.contains(untouched),
            "{untouched}: {edited_text}"
        );
    }
    let checksum = p.checksum("policies/refunds.md");
    p.json(&[
        "move",
        "policies/refunds.md",
        "moved/refunds.md",
        "--expected-checksum",
        &checksum,
    ]);
    assert!(!p.root.join("policies/refunds.md").exists());
    assert_eq!(p.bytes("moved/refunds.md"), edited);
    let checksum = p.checksum("moved/refunds.md");
    let archived = p.json(&[
        "archive",
        "moved/refunds.md",
        "--expected-checksum",
        &checksum,
    ]);
    let path = archived["path"].as_str().unwrap();
    assert!(path.starts_with("archive/"), "{archived}");
    let bytes = p.bytes(path);
    let note = Note::parse(&bytes).unwrap();
    let fm = note.frontmatter().unwrap();
    assert_eq!(fm.get("permalink"), Some(json!("policies/refunds")));
    assert_eq!(fm.get("unknown_list"), Some(json!(["z", "a"])));
    assert_eq!(fm.get("new_string"), Some(json!("yes")));
    assert_eq!(note.relations()[0].target, "notes/target");
    assert_eq!(note.observations()[0].category.as_deref(), Some("fact"));
    assert_eq!(note.wikilinks().len(), 1);
    // Reverse authored links still name the unchanged identity.
    assert_eq!(p.bytes("notes/target.md"), TARGET.as_bytes());
}

#[test]
fn stale_and_invalid_move_refusals_preserve_the_original_project() {
    let p = Project::existing();
    p.init();
    let checksum = p.checksum("policies/refunds.md");
    p.json(&[
        "edit",
        "policies/refunds.md",
        "--append",
        "new version",
        "--expected-checksum",
        &checksum,
    ]);
    let current = p.bytes("policies/refunds.md");
    let stale = p.run(&[
        "edit",
        "policies/refunds.md",
        "--append",
        "stale",
        "--expected-checksum",
        &checksum,
    ]);
    assert!(!stale.status.success());
    let error: Value = serde_json::from_slice(&stale.stdout).unwrap();
    assert_eq!(error["error"]["code"], "stale");
    let checksum = p.checksum("policies/refunds.md");
    for destination in ["../escape.md", "/tmp/escape.md", "notes/target.md"] {
        let refused = p.run(&[
            "move",
            "policies/refunds.md",
            destination,
            "--expected-checksum",
            &checksum,
        ]);
        assert!(!refused.status.success(), "{destination}: {refused:?}");
        assert_eq!(p.bytes("policies/refunds.md"), current);
        assert_eq!(p.bytes("notes/target.md"), TARGET.as_bytes());
    }
}

#[test]
fn observation_relation_and_permalink_scenario_subset() {
    let note = Note::parse(TOPIC.as_bytes()).unwrap();
    assert_eq!(note.observations().len(), 1);
    assert_eq!(note.observations()[0].tags, vec!["policy"]);
    assert_eq!(note.relations().len(), 1);
    assert_eq!(note.relations()[0].rel_type, "relates_to");
    assert_eq!(note.wikilinks().len(), 1); // inline-code link excluded
    let no_semantics =
        Note::parse(b"---\nparse_semantics: false\n---\n- [fact] text\n- relates_to [[Target]]")
            .unwrap();
    assert!(no_semantics.observations().is_empty());
    assert!(no_semantics.relations().is_empty());
    for (path, expected) in [
        ("Notes/Cafe\u{301} Plan.md", "notes/cafe-plan"),
        ("결제/환불 정책.md", "gyeolje/hwanbul-jeongcaeg"),
        ("版本/计划.md", "版本/计划"),
    ] {
        assert_eq!(wi_note::generate_permalink(path), expected);
    }
}

#[test]
fn mcp_context_resolves_existing_plain_relations() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Duration;

    let p = Project::existing();
    p.init();
    // Ensure context starts from a populated index, independent of watcher timing.
    p.json(&["reindex"]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_wi"))
        .arg("mcp")
        .env("WI_VAULT", &p.root)
        .env("WI_CACHE_DIR", &p.cache)
        .env_remove("WI_FAULT")
        .env_remove("WI_NOW")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    // Always stop and reap the process, including on failed assertions.
    struct Stop(std::process::Child);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let stop = Stop(child);
    let mut request = |id: u64, method: &str, params: Value| {
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        input.flush().unwrap();
        loop {
            let line = rx.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                assert!(value.get("error").is_none(), "{value}");
                return value["result"].clone();
            }
        }
    };
    request(
        1,
        "initialize",
        json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"vault-workflows","version":"1"}}),
    );
    // rmcp allows the initialized notification between requests; write it
    // through this helper's input after releasing the mutable closure borrow.
    drop(request);
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    // Startup reconciliation and export generation run in the background
    // after initialize; retry only `generation-changed`, with a deadline.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut id = 2;
    let result: Value = loop {
        writeln!(input, "{}", json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"wi_context","arguments":{"identifier":"policies/refunds.md","depth":1}}})).unwrap();
        input.flush().unwrap();
        let value: Value = loop {
            let line = rx.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                break value;
            }
        };
        let payload: Value =
            serde_json::from_str(value["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let changed = payload["diagnostics"]
            .as_array()
            .is_some_and(|d| d.iter().any(|d| d["code"] == "generation-changed"));
        if !changed || std::time::Instant::now() > deadline {
            break value;
        }
        id += 1;
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(result["result"]["isError"], false, "{result}");
    let payload: Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let hits = payload["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{result}");
    for path in ["policies/refunds.md", "notes/target.md"] {
        let hit = hits.iter().find(|h| h["path"] == path).unwrap();
        assert_eq!(hit["lifecycle"], "none");
    }
    assert_eq!(p.bytes("policies/refunds.md"), TOPIC.as_bytes());
    drop(input);
    drop(stop);
    reader.join().unwrap();
}
