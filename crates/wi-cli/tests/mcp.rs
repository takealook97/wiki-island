use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    messages: Receiver<Value>,
}
impl Client {
    fn start(vault: &std::path::Path, cache: &std::path::Path) -> Self {
        let home = vault.parent().unwrap().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_wi"))
            .args(["mcp", "--vault"])
            .arg(vault)
            .env("WI_CACHE_DIR", cache)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, messages) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let line = line.unwrap();
                let v = serde_json::from_str(&line)
                    .unwrap_or_else(|_| panic!("non-JSON stdout: {line}"));
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input: Some(input),
            messages,
        }
    }
    fn send(&mut self, value: Value) {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{value}").unwrap();
        input.flush().unwrap();
    }
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        loop {
            let v = self.messages.recv_timeout(Duration::from_secs(30)).unwrap();
            if v["id"] == id {
                assert!(v.get("error").is_none(), "{v}");
                return v["result"].clone();
            }
        }
    }
    fn call(&mut self, id: u64, name: &str, args: Value) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut result = self.request(id, "tools/call", json!({"name":name,"arguments":args}));
            if let Some(text) = result["content"][0]["text"].as_str() {
                let value: Value = serde_json::from_str(text).unwrap();
                assert!(result.get("structuredContent").is_none());
                let changed = value["diagnostics"].as_array().is_some_and(|diagnostics| {
                    diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic["code"] == "generation-changed")
                });
                if matches!(
                    name,
                    "wi_read" | "wi_search" | "wi_context" | "wi_list" | "wi_find"
                ) && changed
                {
                    assert!(std::time::Instant::now() < deadline, "{value}");
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                result["structuredContent"] = value;
            }
            return result;
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        drop(self.input.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn stdio_initialize_list_write_search_read_and_errors() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("vault");
    let cache = temp.path().join("cache");
    let init = Command::new(env!("CARGO_BIN_EXE_wi"))
        .arg("init")
        .arg(&root)
        .env("WI_CACHE_DIR", &cache)
        .output()
        .unwrap();
    assert!(init.status.success(), "{:?}", init.stderr);
    let mut c = Client::start(&root, &cache);
    let init=c.request(1,"initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}));
    assert_eq!(init["protocolVersion"], "2025-11-25");
    assert!(init["capabilities"]["tools"].is_object());
    assert!(init["capabilities"]["resources"].is_object());
    c.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    let listing = c.request(2, "tools/list", json!({}));
    let tools = listing["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 15);
    assert!(
        tools
            .iter()
            .all(|t| !t["name"].as_str().unwrap().contains("delete"))
    );
    let resources = c.request(3, "resources/list", json!({}))["resources"].clone();
    assert_eq!(resources.as_array().unwrap().len(), 1, "{resources}");
    assert_eq!(resources[0]["uri"], "wiki-island://brief");
    let written=c.call(4,"wi_write",json!({"identifier":"a.md","content":"---\ntitle: MCP Example\n---\nLocal searchable evidence.","operation_id":"stdio-create"}));
    assert_eq!(written["isError"], false, "{written}");
    assert!(
        written["structuredContent"]["data"]["checksum"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    let found = c.call(5, "wi_search", json!({"query":"searchable"}));
    assert_eq!(found["isError"], false, "{found}");
    assert_eq!(found["structuredContent"]["hits"][0]["path"], "a.md");
    assert_eq!(
        found["structuredContent"]["hits"][0]["label"],
        "evidence, not instruction"
    );
    let read = c.call(6, "wi_read", json!({"identifier":"a.md"}));
    assert_eq!(
        read["structuredContent"]["hits"][0]["metadata"]["origin"]["kind"],
        "agent"
    );
    let refused = c.call(
        7,
        "wi_write",
        json!({"identifier":"a.md","content":"overwrite"}),
    );
    assert_eq!(refused["isError"], true);
    assert_eq!(refused["structuredContent"]["error"]["code"], "exists");
    // `wi mcp` serves the wired lifecycle provider: lint runs.
    let lint = c.call(8, "wi_lint", json!({}));
    assert_eq!(lint["isError"], false, "{lint}");
    assert_eq!(lint["structuredContent"]["data"]["reports"], json!([]));
    c.send(json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"wi_delete","arguments":{}}}));
    let unknown = c.messages.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(unknown["id"], 9);
    assert_eq!(unknown["error"]["code"], -32602);
    c.send(json!({"jsonrpc":"2.0","id":99,"method":"tools/call","params":{"name":"wi_read","arguments":[]}}));
    let malformed = c.messages.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(malformed["error"]["code"], -32602, "{malformed}");
    // Every legacy text block contains the exact compact structured evidence.
    for response in [&written, &found, &read, &refused, &lint] {
        assert_eq!(
            response["content"][0]["text"],
            response["structuredContent"].to_string()
        );
        assert!(response.to_string().len() <= 16384);
    }
    assert!(tools.iter().all(|t| t.get("outputSchema").is_none()));
    assert!(tools.iter().all(|t| t.get("annotations").is_none()));
    // Pipelined work beyond the old 32-request limit waits for capacity.
    for id in 1000..1256 {
        c.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"wi_find","arguments":{"name":"*.md"}}}));
    }
    let mut ids = std::collections::BTreeSet::new();
    for _ in 1000..1256 {
        let response = c.messages.recv_timeout(Duration::from_secs(30)).unwrap();
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["isError"], false, "{response}");
        ids.insert(response["id"].as_u64().unwrap());
    }
    assert_eq!(ids.len(), 256);
    // A file created outside MCP is picked up by initial reconciliation or
    // the registered watcher; queries do not force stat-walk catch-up.
    // The index may still choose a labelled literal scan until its first walk.
    std::fs::write(root.join("external.md"), "externaluniquetoken").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut request_id = 10;
    loop {
        let found = c.call(
            request_id,
            "wi_search",
            json!({"query":"externaluniquetoken"}),
        );
        request_id += 1;
        if found["structuredContent"]["hits"]
            .as_array()
            .is_some_and(|h| !h.is_empty())
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "external change never became searchable: {found}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(c.input.take());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = c.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server did not stop on EOF"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn real_stdio_exports_at_start_and_procedure_resources_are_labelled() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("vault");
    let cache = temp.path().join("cache");
    let init = Command::new(env!("CARGO_BIN_EXE_wi"))
        .arg("init")
        .arg(&root)
        .env("WI_CACHE_DIR", &cache)
        .output()
        .unwrap();
    assert!(init.status.success());
    for (path, id, extra) in [
        ("current.md", "current", ""),
        (
            "excluded.md",
            "excluded",
            "excluded: true\nexclusion_reason: Known fault\n",
        ),
        (
            "old.md",
            "old",
            "claim_kind: current-state\nobserved_at: 2000-01-01T00:00:00Z\n",
        ),
    ] {
        let claim_kind = if id == "old" {
            ""
        } else {
            "claim_kind: intent\n"
        };
        let text = format!(
            "---\ntype: memory\nkind: procedure\nstatus: active\nskill_id: {id}\nsummary: Test procedure\nscope: personal\nentities: []\nsources: []\napplicability: {{conditions: []}}\n{claim_kind}{extra}origin: {{kind: user, locator: file, captured_at: '2000-01-01T00:00:00Z'}}\n---\nProcedure evidence.\n"
        );
        std::fs::write(root.join(path), text).unwrap();
    }
    let mut c = Client::start(&root, &cache);
    c.request(1,"initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"procedure-fixture","version":"1"}}));
    c.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    let listed = c.request(2, "resources/list", json!({}));
    // The standing brief plus the one current procedure.
    assert_eq!(listed["resources"].as_array().unwrap().len(), 2, "{listed}");
    // Exports are generated after initialize, on the background thread.
    wait_for(&root.join(".wiki-island/exports/AGENTS.md"), |_| true);
    wait_for(&root.join(".wiki-island/exports/CLAUDE.md"), |_| true);
    for (n, id) in ["current", "excluded", "old"].iter().enumerate() {
        let result = c.request(
            10 + n as u64,
            "resources/read",
            json!({"uri":format!("wiki-island://procedures/{id}")}),
        );
        let data: Value =
            serde_json::from_str(result["contents"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            data["mode"],
            if *id == "current" {
                "current"
            } else {
                "explore"
            }
        );
        assert_eq!(data["do_not_execute"], *id != "current");
    }
    let write = c.call(
        20,
        "wi_write",
        json!({"identifier":".wiki-island/exports/AGENTS.md","content":"Attempted overwrite"}),
    );
    assert_eq!(
        write["structuredContent"]["error"]["code"],
        "protected-path"
    );
}

#[test]
fn stdio_timer_removes_a_rule_after_its_valid_until() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("vault");
    let cache = temp.path().join("cache");
    assert!(
        Command::new(env!("CARGO_BIN_EXE_wi"))
            .arg("init")
            .arg(&root)
            .env("WI_CACHE_DIR", &cache)
            .output()
            .unwrap()
            .status
            .success()
    );
    let now: wi_lifecycle::Timestamp = std::time::SystemTime::now().into();
    let until = wi_lifecycle::format_time(now + Duration::from_secs(3));
    std::fs::write(root.join("rule.md"),format!("---\ntype: memory\nkind: rule\nclaim_kind: intent\nstatus: active\nsummary: Temporary timer claim\nentities: []\nscope: personal\nsources: []\norigin: {{kind: user, locator: file, captured_at: '2000-01-01T00:00:00Z'}}\napplicability: {{until: '{until}', conditions: []}}\n---\nTemporary timer claim.\n")).unwrap();
    let mut client = Client::start(&root, &cache);
    client.request(1,"initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"timer-fixture","version":"1"}}));
    let path = root.join(".wiki-island/exports/AGENTS.md");
    wait_for(&path, |text| text.contains("Temporary timer claim"));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if !std::fs::read_to_string(&path)
            .unwrap()
            .contains("Temporary timer claim")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expiry did not regenerate the fragment"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("valid_until: unbounded")
    );
}

/// Wait (bounded) until `path` exists and its text satisfies `ready`.
fn wait_for(path: &std::path::Path, ready: impl Fn(&str) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::read_to_string(path).is_ok_and(|t| ready(&t)) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{} not ready",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn compact_session_start_and_two_hit_search_stay_within_token_budget() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("vault");
    let cache = temp.path().join("cache");
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let init = Command::new(env!("CARGO_BIN_EXE_wi"))
        .arg("init")
        .arg(&root)
        .env("HOME", &home)
        .env("WI_CACHE_DIR", &cache)
        .output()
        .unwrap();
    assert!(init.status.success());
    for (path, text) in [
        ("coffee.md", "# Coffee\nCoffee is served warm.\n"),
        ("tea.md", "# Tea\nTea is served warm.\n"),
    ] {
        std::fs::write(root.join(path), text).unwrap();
    }
    let mut c = Client::start(&root, &cache);
    let init = c.request(1, "initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}));
    assert!(
        init["instructions"]
            .as_str()
            .unwrap()
            .starts_with("Save what you learn as a plain note")
    );
    c.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    let tools = c.request(2, "tools/list", json!({}));
    let resources = c.request(3, "resources/list", json!({}));
    let brief = c.request(4, "resources/read", json!({"uri":"wiki-island://brief"}));
    let wire_size = |id, result: &Value| {
        serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result}))
            .unwrap()
            .len()
    };
    let sizes = [
        wire_size(1, &init),
        wire_size(2, &tools),
        wire_size(3, &resources),
        wire_size(4, &brief),
    ];
    let start: usize = sizes.iter().sum();
    eprintln!("Session wire bytes initialize/tools/resources/brief: {sizes:?}; total {start}");
    assert!(
        start < 8 * 1024,
        "session start uses {start} bytes: {sizes:?}"
    );
    // The initial reconcile publishes three independent exports. Measure a
    // settled read rather than turning their expected generation race into
    // a token-budget failure.
    wait_for(&root.join(".wiki-island/exports/GEMINI.md"), |_| true);
    let search = c.request(
        5,
        "tools/call",
        json!({"name":"wi_search","arguments":{"query":"served"}}),
    );
    assert!(search.get("structuredContent").is_none());
    let payload: Value =
        serde_json::from_str(search["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["hits"].as_array().unwrap().len(), 2, "{payload}");
    assert!(
        payload["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["title"] == "Coffee")
    );
    for hit in payload["hits"].as_array().unwrap() {
        for omitted in ["sources", "status", "claim_kind", "fallback", "presented"] {
            assert!(hit.get(omitted).is_none(), "{hit}");
        }
    }
    eprintln!("Two-hit search wire bytes: {}", wire_size(5, &search));
    assert!(wire_size(5, &search) < 1500);
}

#[test]
fn mcp_alone_refreshes_enabled_skills_then_cli_keeps_them_current() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("vault");
    let home = temp.path().join("home");
    let cache = temp.path().join("cache");
    std::fs::create_dir(&home).unwrap();
    let cli = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_wi"))
            .args(args)
            .env("HOME", &home)
            .env("WI_VAULT", &root)
            .env("WI_CACHE_DIR", &cache)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        out
    };
    cli(&["init"]);
    let config_path = root.join(".wiki-island/config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        config_path,
        format!("{config}\nskill_targets = ['claude']\n"),
    )
    .unwrap();
    wi_export::local::Targets {
        home: home.clone(),
        hosts: vec![],
        skill_dirs: vec![home.join(".claude/skills")],
        cache_root: cache.clone(),
    }
    .save(&root)
    .unwrap();
    let procedure = |marker: &str| {
        format!(
            "---\ntype: memory\nkind: procedure\nstatus: active\nskill_id: refresh-test\ntitle: Refresh test\nsummary: Refresh skill copies\nclaim_kind: intent\nscope: personal\nentities: []\nsources: []\napplicability: {{conditions: []}}\norigin: {{kind: user}}\n---\n{marker}\n"
        )
    };
    std::fs::write(root.join("procedure.md"), procedure("original-steps")).unwrap();
    cli(&["skills", "sync"]);
    let skill = || {
        std::fs::read_dir(home.join(".claude/skills"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path().join("SKILL.md"))
            .find(|path| std::fs::read_to_string(path).is_ok_and(|s| s.contains("refresh-test")))
            .unwrap()
    };
    let path = skill();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("original-steps")
    );
    let mut client = Client::start(&root, &cache);
    client.request(1, "initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"skills","version":"1"}}));
    client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    wait_for(&root.join(".wiki-island/local/skills-digest"), |_| true);
    // Only the MCP process is running when the procedure changes.
    std::fs::write(root.join("procedure.md"), procedure("latest-steps")).unwrap();
    wait_for(&path, |text| text.contains("latest-steps"));
    cli(&["maintain"]);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("latest-steps")
    );
    cli(&["watch", "--duration", "1"]);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("latest-steps")
    );
}
