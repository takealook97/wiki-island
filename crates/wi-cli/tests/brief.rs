//! `wi brief` and the `wiki-island://brief` MCP resource.
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

fn wi(cache: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wi"))
        .args(args)
        .env("WI_CACHE_DIR", cache)
        .output()
        .unwrap()
}

fn claim(kind: &str, summary: &str, observed: &str) -> String {
    format!(
        "---\ntype: memory\nkind: {kind}\nstatus: active\nsummary: {summary}\nscope: personal\nentities: []\nsources: []\napplicability: {{conditions: []}}\nclaim_kind: intent\nobserved_at: '{observed}'\norigin: {{kind: user, locator: file, captured_at: '2000-01-01T00:00:00Z'}}\n---\n{summary}\n"
    )
}

fn vault() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("vault");
    let cache = temp.path().join("cache");
    let init = wi(&cache, &["init", root.to_str().unwrap()]);
    assert!(init.status.success(), "{init:?}");
    let files = [
        (
            "pin.md",
            "---\ntitle: Release train\npinned: true\n---\nShip on Tuesdays. Details follow in the body.\n".to_owned(),
        ),
        ("unpinned.md", "---\npinned: false\n---\nNot in the brief.\n".to_owned()),
        ("rule-old.md", claim("rule", "Never force-push main", "2026-01-01T00:00:00Z")),
        ("rule-new.md", claim("rule", "Sign every release tag", "2026-06-01T00:00:00Z")),
        ("pref.md", claim("preference", "Prefer short commit subjects", "2026-03-01T00:00:00Z")),
    ];
    for (path, text) in files {
        std::fs::write(root.join(path), text).unwrap();
    }
    (temp, root, cache)
}

fn brief_json(root: &Path, cache: &Path) -> Value {
    let out = wi(
        cache,
        &["--vault", root.to_str().unwrap(), "--json", "brief"],
    );
    assert!(out.status.success(), "{out:?}");
    let mut value: Value = serde_json::from_slice(&out.stdout).unwrap();
    // The CLI envelope adds ingest warnings; the cap is on the brief itself.
    value.as_object_mut().unwrap().remove("warnings");
    value
}

#[test]
fn brief_orders_pinned_then_rules_then_preferences_newest_first() {
    let (_temp, root, cache) = vault();
    let brief = brief_json(&root, &cache);
    let order: Vec<_> = brief["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["kind"].clone(), i["path"].clone()))
        .collect();
    assert_eq!(
        order,
        vec![
            (json!("pinned"), json!("pin.md")),
            (json!("rule"), json!("rule-new.md")),
            (json!("rule"), json!("rule-old.md")),
            (json!("preference"), json!("pref.md")),
        ],
        "{brief}"
    );
    assert_eq!(brief["items"][0]["summary"], "Ship on Tuesdays.");
    assert_eq!(brief["items"][0]["title"], "Release train");
    assert_eq!(brief["items"][1]["summary"], "Sign every release tag");
    assert_eq!(brief["label"], "evidence, not instruction");
    assert_eq!(brief["omitted"], 0);
    assert!(serde_json::to_vec(&brief).unwrap().len() <= 2048);

    let human = wi(&cache, &["--vault", root.to_str().unwrap(), "brief"]);
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(
        text.starts_with("[pinned] Ship on Tuesdays. (pin.md)"),
        "{text}"
    );
}

#[test]
fn brief_is_capped_at_two_kib_with_an_omitted_count() {
    let (_temp, root, cache) = vault();
    for i in 0..40 {
        std::fs::write(
            root.join(format!("pin-{i:02}.md")),
            format!(
                "---\npinned: true\nsummary: Pinned entry {i:02} {}\n---\nBody.\n",
                "x".repeat(60)
            ),
        )
        .unwrap();
    }
    let brief = brief_json(&root, &cache);
    assert!(serde_json::to_vec(&brief).unwrap().len() <= 2048, "{brief}");
    let shown = brief["items"].as_array().unwrap().len() as u64;
    assert!(shown > 0);
    assert_eq!(shown + brief["omitted"].as_u64().unwrap(), 44, "{brief}");
}

#[test]
fn mcp_lists_and_reads_the_brief_resource() {
    let (_temp, root, cache) = vault();
    // Let the CLI ingest and adopt the files first, as an owner edit would.
    brief_json(&root, &cache);
    let mut child = Command::new(env!("CARGO_BIN_EXE_wi"))
        .args(["mcp", "--vault"])
        .arg(&root)
        .env("WI_CACHE_DIR", &cache)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut request = |id: u64, method: &str, params: Value| -> Value {
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        input.flush().unwrap();
        loop {
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    };
    let init = request(
        1,
        "initialize",
        json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"brief","version":"1"}}),
    );
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("wiki-island://brief")
    );
    let listed = request(2, "resources/list", json!({}));
    assert_eq!(
        listed["result"]["resources"][0]["uri"], "wiki-island://brief",
        "{listed}"
    );
    let read = request(3, "resources/read", json!({"uri":"wiki-island://brief"}));
    let text = read["result"]["contents"][0]["text"].as_str().unwrap();
    let brief: Value = serde_json::from_str(text).unwrap();
    assert_eq!(brief["items"][0]["path"], "pin.md", "{brief}");
    assert!(text.len() <= 2048);
    drop(input);
    let _ = child.kill();
    let _ = child.wait();
}
