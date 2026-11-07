//! Model-free timeline scenario. One fixture of ordered steps drives the real
//! `wi` binary through the CLI and MCP stdio against a temporary HOME and vault.
//! After every step a pure rules judge checks what was delivered or withheld
//! and names the layer that failed: capture, index, lifecycle gate, delivery,
//! maintenance or integration. No model, no network, no real HOME.
//!
//! The CLI reports engine withheld reasons (`not-active`); MCP collapses them to the
//! protocol vocabulary (`not-current`), so each rule names the reasons of its own surface.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const TIMELINE: &str = include_str!("fixtures/timeline.json");

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/timeline")
}

/// A failed rule: the layer that is to blame and what was observed.
struct Verdict {
    layer: String,
    step: String,
    message: String,
}

struct World {
    temp: TempDir,
    now: String,
    hosts: Vec<(String, String)>,
    /// What the last step observed, for rules that judge a whole step.
    mcp_replies: Vec<Reply>,
    brief_resource: Option<String>,
    maintain: Vec<(Value, String, String)>,
}

struct Reply {
    tool: String,
    structured: Value,
    bytes: usize,
    is_error: bool,
}

impl World {
    fn new(plan: &Value) -> Self {
        let temp = TempDir::new().unwrap();
        for dir in ["home", "bin", "cache"] {
            std::fs::create_dir_all(temp.path().join(dir)).unwrap();
        }
        let world = Self {
            now: plan["now"].as_str().unwrap().to_owned(),
            hosts: plan["hosts"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect(),
            temp,
            mcp_replies: Vec::new(),
            brief_resource: None,
            maintain: Vec::new(),
        };
        std::fs::copy(env!("CARGO_BIN_EXE_wi"), world.path("bin/wi")).unwrap();
        let fake = world.path("bin/launchctl");
        std::fs::write(
            &fake,
            "#!/bin/sh\ncase $1 in\nprint) test -f \"$HOME/loaded\";;\nbootstrap) touch \"$HOME/loaded\";;\nbootout) rm -f \"$HOME/loaded\";;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        for (path, bytes) in &world.hosts {
            world.seed(&format!("home/{path}"), bytes.as_bytes());
        }
        world
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.temp.path().join(rel)
    }

    fn vault(&self) -> PathBuf {
        self.path("vault")
    }

    fn seed(&self, rel: &str, bytes: &[u8]) {
        let path = self.path(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn command(&self, binary: &Path) -> Command {
        let mut cmd = Command::new(binary);
        cmd.env("HOME", self.path("home"))
            .env("CODEX_HOME", self.path("home/.codex"))
            .env("WI_VAULT", self.vault())
            .env("WI_CACHE_DIR", self.path("cache"))
            .env("WI_LAUNCHCTL", self.path("bin/launchctl"))
            .env("WI_NOW", &self.now)
            .env_remove("WI_FAULT");
        cmd
    }

    fn wi(&self, args: &[&str]) -> Output {
        self.command(&self.path("bin/wi"))
            .args(args)
            .output()
            .unwrap()
    }

    /// `wi --json <args>`: parsed stdout and success.
    fn wi_json(&self, args: &[&str]) -> (bool, Value) {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.wi(&all);
        (
            out.status.success(),
            serde_json::from_slice(&out.stdout).unwrap_or(Value::Null),
        )
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.vault())
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.vault().join(rel)).ok()
    }

    /// Replace an `@fixture/name` reference with the fixture path.
    fn arg(&self, text: &str) -> String {
        match text.strip_prefix("@fixture/") {
            Some(name) => fixture_dir().join(name).to_string_lossy().into_owned(),
            None => text.to_owned(),
        }
    }

    /// An editor saves bytes; the next one-shot command ingests them.
    fn editor_write(&self, rel: &str, bytes: &[u8]) -> Result<(), String> {
        let path = self.vault().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        let out = self.wi(&["commit"]);
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("wi commit failed: {}", lossy(&out.stderr)))
        }
    }

    fn run(&mut self, action: &Value) -> Result<(), String> {
        match action["op"].as_str().unwrap() {
            "sequence" => {
                for step in action["steps"].as_array().unwrap() {
                    self.run(step)?;
                }
                Ok(())
            }
            "cli" => {
                let args: Vec<String> = action["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| self.arg(a.as_str().unwrap()))
                    .collect();
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                let out = self.wi(&refs);
                if out.status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "`wi {}` failed: {}",
                        args.join(" "),
                        lossy(&out.stderr)
                    ))
                }
            }
            "editor_write" => {
                let rel = action["path"].as_str().unwrap();
                let mut bytes = match action["fixture"].as_str() {
                    Some(name) => std::fs::read(fixture_dir().join(name)).unwrap(),
                    None => action["text"].as_str().unwrap().as_bytes().to_vec(),
                };
                if let Some(pin) = action["pin"].as_str() {
                    let source = self.read(pin).ok_or("pinned source is missing")?;
                    let hash = format!("sha256:{}", sha256_hex(&source));
                    let text = String::from_utf8(bytes)
                        .unwrap()
                        .replace("{{HASH}}", &hash)
                        .replace("{{LEN}}", &source.len().to_string());
                    bytes = text.into_bytes();
                }
                self.editor_write(rel, &bytes)
            }
            "obsidian_churn" => {
                let token = action["token"].as_str().unwrap();
                for (rel, body) in [
                    (
                        ".obsidian/workspace.json",
                        format!("{{\"last\":\"{token}\"}}"),
                    ),
                    (
                        ".obsidian/plugins/x/data.json",
                        format!("{{\"k\":\"{token}\"}}"),
                    ),
                    (".trash/old.md", format!("trashed {token}\n")),
                ] {
                    self.editor_write(rel, body.as_bytes())?;
                }
                Ok(())
            }
            "notion_zip" => {
                let page = action["page"].as_str().unwrap();
                let name = "Home 11111111111111111111111111111111.md";
                self.seed(
                    "vault/notion-export.zip",
                    &stored_zip(&[(name, page.as_bytes())]),
                );
                let out = self.wi(&["import", "notion-export.zip", "--ocr", "off"]);
                if out.status.success() {
                    Ok(())
                } else {
                    Err(format!("import failed: {}", lossy(&out.stderr)))
                }
            }
            "maintain_twice" => {
                self.maintain.clear();
                for _ in 0..2 {
                    let before = self.git(&["rev-parse", "HEAD"]);
                    let (ok, report) = self.wi_json(&["maintain"]);
                    if !ok {
                        return Err(format!("maintain failed: {report}"));
                    }
                    let after = self.git(&["rev-parse", "HEAD"]);
                    self.maintain.push((report, before, after));
                }
                Ok(())
            }
            "mcp" => self.run_mcp(action),
            other => Err(format!("unknown op {other}")),
        }
    }

    fn run_mcp(&mut self, action: &Value) -> Result<(), String> {
        // Ingest editor and CLI changes first so the fresh server starts from a current index.
        self.wi(&["commit"]);
        let mut client = Client::start(self);
        self.mcp_replies.clear();
        for call in action["calls"].as_array().unwrap() {
            let tool = call["tool"].as_str().unwrap();
            let mut args = call["args"].clone();
            for value in args.as_object_mut().unwrap().values_mut() {
                if let Some(name) = value.as_str().and_then(|s| s.strip_prefix("@fixture/")) {
                    *value = json!(std::fs::read_to_string(fixture_dir().join(name)).unwrap());
                }
            }
            self.mcp_replies.push(client.call(tool, args)?);
        }
        if action["brief_resource"] == true {
            self.brief_resource = Some(client.brief()?);
        }
        client.close();
        Ok(())
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

fn sha256_hex(bytes: &[u8]) -> String {
    wi_lifecycle::sha256(bytes)
        .trim_start_matches("sha256:")
        .to_owned()
}

/// The frontmatter value at a dotted key, from the bytes on disk.
fn frontmatter_value(bytes: &[u8], key: &str) -> Option<Value> {
    let note = wi_note::Note::parse(bytes).ok()?;
    let mut parts = key.split('.');
    let mut value = note.frontmatter()?.get(parts.next()?)?;
    for part in parts {
        value = value.get(part)?.clone();
    }
    Some(value)
}

// ---- MCP stdio client -------------------------------------------------------

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    messages: Receiver<Value>,
    next: u64,
}

impl Client {
    fn start(world: &World) -> Self {
        let mut child = world
            .command(&world.path("bin/wi"))
            .args(["mcp", "--vault"])
            .arg(world.vault())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, messages) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let mut client = Self {
            child,
            input: Some(input),
            messages,
            next: 0,
        };
        client
            .request(
                "initialize",
                json!({"protocolVersion":"2025-11-25","capabilities":{},
                       "clientInfo":{"name":"timeline","version":"1"}}),
            )
            .expect("initialize");
        let input = client.input.as_mut().unwrap();
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        input.flush().unwrap();
        client
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let input = self.input.as_mut().unwrap();
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        input.flush().unwrap();
        loop {
            let v = self
                .messages
                .recv_timeout(Duration::from_secs(30))
                .map_err(|e| format!("no MCP reply to {method}: {e}"))?;
            if v["id"] == id {
                if let Some(error) = v.get("error") {
                    return Err(format!("MCP error for {method}: {error}"));
                }
                return Ok(v["result"].clone());
            }
        }
    }

    fn call(&mut self, tool: &str, args: Value) -> Result<Reply, String> {
        let result = self.request("tools/call", json!({"name": tool, "arguments": args}))?;
        Ok(Reply {
            tool: tool.to_owned(),
            bytes: result.to_string().len(),
            is_error: result["isError"] == true,
            structured: serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
                .unwrap(),
        })
    }

    fn brief(&mut self) -> Result<String, String> {
        let result = self.request("resources/read", json!({"uri": "wiki-island://brief"}))?;
        Ok(result["contents"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_owned())
    }

    /// End the session cleanly (EOF), killing it only if it hangs.
    fn close(mut self) {
        drop(self.input.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---- The judge: pure rules over observed state -----------------------------------

struct Judge<'a> {
    world: &'a World,
    step: &'a str,
    verdicts: Vec<Verdict>,
}

impl Judge<'_> {
    fn fail(&mut self, layer: &str, message: String) {
        self.verdicts.push(Verdict {
            layer: layer.to_owned(),
            step: self.step.to_owned(),
            message,
        });
    }

    fn rule(&mut self, rule: &Value) {
        let layer = rule["layer"].as_str().unwrap();
        let check = rule["check"].as_str().unwrap();
        let text = |key: &str| rule[key].as_str().unwrap().to_owned();
        match check {
            "file_exists" => {
                let rel = text("path");
                let path = if let Some(home) = rel.strip_prefix("@home/") {
                    self.world.path("home").join(home)
                } else {
                    self.world.vault().join(&rel)
                };
                let want = rule["exists"].as_bool().unwrap();
                if path.exists() != want {
                    self.fail(layer, format!("{rel}: exists should be {want}"));
                }
            }
            "file_contains" => {
                let rel = text("path");
                let want = text("text");
                let path = match rel.strip_prefix("@home/") {
                    Some(home) => self.world.path("home").join(home),
                    None => self.world.vault().join(&rel),
                };
                let body = std::fs::read_to_string(path).unwrap_or_default();
                if !body.contains(&want) {
                    self.fail(layer, format!("{rel} lacks {want:?}"));
                }
            }
            "frontmatter" => {
                let (rel, key) = (text("path"), text("key"));
                let found = self
                    .world
                    .read(&rel)
                    .and_then(|bytes| frontmatter_value(&bytes, &key));
                if found.as_ref() != Some(&rule["equals"]) {
                    self.fail(
                        layer,
                        format!("{rel} {key} is {found:?}, expected {}", rule["equals"]),
                    );
                }
            }
            "delivered" => self.delivered(layer, rule),
            "git_log_lacks" => {
                let want = text("text");
                let log = self.world.git(&["log", "--name-only", "--format=%s"]);
                if log.contains(&want) {
                    self.fail(layer, format!("git history mentions {want:?}"));
                }
            }
            "brief" => self.brief(layer, rule),
            "work_next" => {
                let kinds = self.world.work_kinds();
                for kind in rule["kinds"].as_array().unwrap() {
                    if !kinds.contains(kind.as_str().unwrap()) {
                        self.fail(layer, format!("no {kind} work item in {kinds:?}"));
                    }
                }
            }
            "maintain_idempotent" => self.idempotent(layer),
            "mcp_ok" => {
                let bad: Vec<_> = self
                    .world
                    .mcp_replies
                    .iter()
                    .filter(|r| r.is_error)
                    .map(|r| r.tool.clone())
                    .collect();
                if bad.is_empty() && self.world.mcp_replies.is_empty() {
                    self.fail(layer, "no MCP replies were recorded".to_owned());
                }
                if !bad.is_empty() {
                    self.fail(layer, format!("MCP tools returned errors: {bad:?}"));
                }
            }
            "mcp_bytes" | "work_next_bytes" => {
                let cap = rule["max_bytes"].as_u64().unwrap() as usize;
                for reply in &self.world.mcp_replies {
                    let relevant = check == "mcp_bytes" || reply.tool == "wi_work";
                    if relevant && reply.bytes > cap {
                        self.fail(
                            layer,
                            format!("{} delivered {} bytes, cap {cap}", reply.tool, reply.bytes),
                        );
                    }
                }
            }
            "hosts_restored" => {
                for (path, bytes) in &self.world.hosts {
                    let now = std::fs::read_to_string(self.world.path("home").join(path));
                    if now.as_deref().ok() != Some(bytes.as_str()) {
                        self.fail(layer, format!("host file {path} is {now:?}"));
                    }
                }
                for left in ["home/.claude/skills", "home/.agents/skills", "home/loaded"] {
                    if self.world.path(left).exists() {
                        self.fail(layer, format!("uninstall left {left}"));
                    }
                }
            }
            other => self.fail("judge", format!("unknown check {other}")),
        }
    }

    /// Delivered or withheld, with the layer refined from what went wrong.
    fn delivered(&mut self, layer: &str, rule: &Value) {
        let query = rule["query"].as_str().unwrap();
        let (paths, permalinks, withheld, bytes) = match rule["via"].as_str().unwrap() {
            "cli" => self.world.cli_search(query),
            _ => self.world.mcp_search(query),
        };
        if let Some(min) = rule["min_hits"].as_u64() {
            if (paths.len() as u64) < min {
                self.fail(
                    layer,
                    format!("{query:?}: {} hits, expected {min}+", paths.len()),
                );
            }
            return;
        }
        let want: BTreeSet<String> = strings(&rule["paths"]);
        let want_withheld: BTreeSet<String> = strings(&rule["withheld"]);
        let got: BTreeSet<String> = paths.iter().cloned().collect();
        let via = rule["via"].as_str().unwrap();
        for missing in want.difference(&got) {
            // Missing and not withheld: the index lost it. Withheld: a gate did.
            let blame = if withheld.is_empty() {
                "index"
            } else {
                "lifecycle gate"
            };
            self.fail(
                blame,
                format!("{via} {query:?}: {missing} not delivered (withheld {withheld:?})"),
            );
        }
        for extra in got.difference(&want) {
            self.fail(
                "lifecycle gate",
                format!("{via} {query:?}: {extra} delivered but must be withheld or absent"),
            );
        }
        if withheld != want_withheld {
            self.fail(
                "lifecycle gate",
                format!("{via} {query:?}: withheld {withheld:?}, expected {want_withheld:?}"),
            );
        }
        if let Some(links) = rule["permalinks"].as_object() {
            for (path, link) in links {
                if permalinks.get(path.as_str()).map(String::as_str) != link.as_str() {
                    self.fail(
                        "index",
                        format!(
                            "{path}: permalink {:?}, expected {link}",
                            permalinks.get(path.as_str())
                        ),
                    );
                }
            }
        }
        if bytes > 16384 {
            self.fail(
                "delivery",
                format!("{via} {query:?}: {bytes} bytes over 16384"),
            );
        }
    }

    fn brief(&mut self, layer: &str, rule: &Value) {
        let cap = rule["max_bytes"].as_u64().unwrap() as usize;
        let (ok, brief) = self.world.wi_json(&["brief"]);
        let mut sources = vec![("cli", brief.to_string())];
        if !ok {
            self.fail(layer, "wi brief failed".to_owned());
        }
        if let Some(resource) = &self.world.brief_resource {
            sources.push(("mcp resource", resource.clone()));
        }
        for (via, text) in sources {
            if text.len() > cap {
                self.fail(
                    layer,
                    format!("{via} brief is {} bytes, cap {cap}", text.len()),
                );
            }
            for want in rule["contains"].as_array().unwrap() {
                if !text.contains(want.as_str().unwrap()) {
                    self.fail(layer, format!("{via} brief lacks {want}"));
                }
            }
            for forbidden in rule["excludes"].as_array().unwrap() {
                if text.contains(forbidden.as_str().unwrap()) {
                    self.fail("lifecycle gate", format!("{via} brief carries {forbidden}"));
                }
            }
            // Protected items only: an agent's noted note has its own section.
            let protected = serde_json::from_str::<Value>(&text)
                .map(|brief| brief["items"].to_string())
                .unwrap_or(text);
            for forbidden in rule["protected_excludes"].as_array().into_iter().flatten() {
                if protected.contains(forbidden.as_str().unwrap()) {
                    self.fail(
                        "lifecycle gate",
                        format!("{via} brief protects {forbidden}"),
                    );
                }
            }
        }
    }

    fn idempotent(&mut self, layer: &str) {
        let runs = &self.world.maintain;
        let mut problems = Vec::new();
        if runs.len() != 2 {
            problems.push("maintain did not run twice".to_owned());
        }
        if let Some((report, before, after)) = runs.get(1) {
            for key in ["repaired", "work_created", "work_closed", "migrated"] {
                if report[key] != 0 {
                    problems.push(format!("second maintain: {key} = {}", report[key]));
                }
            }
            for key in ["index_rebuilt", "procedures_changed"] {
                if report[key] != false {
                    problems.push(format!("second maintain: {key} = {}", report[key]));
                }
            }
            if before != after {
                problems.push("second maintain made a commit".to_owned());
            }
        }
        if !self.world.git(&["status", "--porcelain"]).trim().is_empty() {
            problems.push("vault has uncommitted changes after maintain".to_owned());
        }
        for problem in problems {
            self.fail(layer, problem);
        }
    }
}

fn strings(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

type Delivery = (
    Vec<String>,
    std::collections::BTreeMap<String, String>,
    BTreeSet<String>,
    usize,
);

impl World {
    fn cli_search(&self, query: &str) -> Delivery {
        let out = self.wi(&["--json", "search", query]);
        let body: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        let hits = body["hits"].as_array().cloned().unwrap_or_default();
        let paths = hits
            .iter()
            .filter_map(|h| h["rel_path"].as_str().map(str::to_owned))
            .collect();
        let links = hits
            .iter()
            .filter_map(|h| {
                Some((
                    h["rel_path"].as_str()?.to_owned(),
                    h["permalink"].as_str()?.to_owned(),
                ))
            })
            .collect();
        (paths, links, withheld_reasons(&body), out.stdout.len())
    }

    fn mcp_search(&self, query: &str) -> Delivery {
        self.wi(&["commit"]);
        let mut client = Client::start(self);
        let reply = client.call("wi_search", json!({"query": query}));
        client.close();
        let Ok(reply) = reply else {
            return (
                Vec::new(),
                Default::default(),
                BTreeSet::from(["mcp-error".to_owned()]),
                0,
            );
        };
        let hits = reply.structured["hits"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let paths = hits
            .iter()
            .filter_map(|h| h["path"].as_str().map(str::to_owned))
            .collect();
        let links = hits
            .iter()
            .filter_map(|h| {
                Some((
                    h["path"].as_str()?.to_owned(),
                    h["permalink"].as_str()?.to_owned(),
                ))
            })
            .collect();
        (
            paths,
            links,
            withheld_reasons(&reply.structured),
            reply.bytes,
        )
    }

    /// Kinds of the open work items, from `wi_work next`/`list` over MCP replies.
    fn work_kinds(&self) -> BTreeSet<String> {
        self.mcp_replies
            .iter()
            .filter(|r| r.tool == "wi_work")
            .flat_map(|r| {
                r.structured["data"]["items"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|i| i["kind"].as_str().map(str::to_owned))
            .collect()
    }
}

fn withheld_reasons(body: &Value) -> BTreeSet<String> {
    body["withheld"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|w| w["reason"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A stored (uncompressed) ZIP archive, enough for a synthetic Notion export.
fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, bytes) in entries {
        let offset = out.len() as u32;
        let (crc, size, len) = (crc32(bytes), bytes.len() as u32, name.len() as u16);
        out.extend_from_slice(b"PK\x03\x04\x14\x00\x00\x08\x00\x00\x00\x00\x00\x00");
        for v in [crc, size, size] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(bytes);
        central.extend_from_slice(b"PK\x01\x02\x14\x00\x14\x00\x00\x08\x00\x00\x00\x00\x00\x00");
        for v in [crc, size, size] {
            central.extend_from_slice(&v.to_le_bytes());
        }
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let (start, size, count) = (out.len() as u32, central.len() as u32, entries.len() as u16);
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00");
    for v in [count, count] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in [size, start] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Run the whole timeline; every failed rule is reported with its layer.
fn run_timeline() -> Vec<Verdict> {
    let plan: Value = serde_json::from_str(TIMELINE).unwrap();
    let mut world = World::new(&plan);
    let mut verdicts = Vec::new();
    for step in plan["steps"].as_array().unwrap() {
        let id = step["id"].as_str().unwrap();
        let judge_rules: Vec<&Value> = step["expect"].as_array().unwrap().iter().collect();
        let started = Instant::now();
        if let Err(message) = world.run(&step["do"]) {
            let layer = judge_rules
                .first()
                .map_or("integration", |r| r["layer"].as_str().unwrap());
            verdicts.push(Verdict {
                layer: layer.to_owned(),
                step: id.to_owned(),
                message: format!("step could not run: {message}"),
            });
            continue;
        }
        let mut judge = Judge {
            world: &world,
            step: id,
            verdicts: Vec::new(),
        };
        for rule in judge_rules {
            judge.rule(rule);
        }
        verdicts.append(&mut judge.verdicts);
        eprintln!("step {id}: {:?}", started.elapsed());
    }
    verdicts
}

fn report(verdicts: &[Verdict]) -> String {
    verdicts
        .iter()
        .map(|v| format!("[{}] {}: {}", v.layer, v.step, v.message))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn timeline_judged_by_layer() {
    let started = Instant::now();
    let verdicts = run_timeline();
    assert!(
        verdicts.is_empty(),
        "timeline failed:\n{}",
        report(&verdicts)
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "timeline took {:?}",
        started.elapsed()
    );
}
