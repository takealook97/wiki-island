//! Several vaults on one machine: one app entry per vault, set up and removed independently.
#![cfg(unix)]
mod support;
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;
use wi_export::hosts::HostId;

/// Hosts detected in the fake home: one JSON host of each common shape and Codex TOML.
const HOSTS: [HostId; 3] = [HostId::Claude, HostId::Codex, HostId::Cursor];

struct Fixture {
    temp: TempDir,
}
impl Fixture {
    fn new() -> Self {
        let f = Self {
            temp: tempfile::tempdir().unwrap(),
        };
        for dir in ["home/.claude", "home/.codex", "home/.cursor", "bin", "data"] {
            fs::create_dir_all(f.path(dir)).unwrap();
        }
        fs::copy(env!("CARGO_BIN_EXE_wi"), f.path("bin/wi")).unwrap();
        // Service fakes: launchd on macOS, a systemd user manager on Linux.
        f.script(
            "bin/launchctl",
            "#!/bin/sh\ncase $1 in\nprint) test -f \"$HOME/loaded\";;\nbootstrap) touch \"$HOME/loaded\";;\nbootout) rm -f \"$HOME/loaded\";;\nesac\n",
        );
        f.script(
            "bin/systemctl",
            concat!(
                "#!/bin/sh\nshift\ncase $1 in\n",
                "show-environment) exit 0;;\n",
                "is-enabled) if test -f \"$HOME/enabled\"; then echo enabled; else echo disabled; exit 1; fi; exit 0;;\n",
                "is-active) if test -f \"$HOME/active\"; then echo active; else echo inactive; exit 3; fi; exit 0;;\n",
                "enable) touch \"$HOME/enabled\";;\n",
                "disable) rm -f \"$HOME/enabled\";;\n",
                "start|restart) touch \"$HOME/active\";;\n",
                "stop) rm -f \"$HOME/active\";;\n",
                "esac\nexit 0\n",
            ),
        );
        f
    }
    fn script(&self, path: &str, text: &str) {
        fs::write(self.path(path), text).unwrap();
        fs::set_permissions(self.path(path), fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn path(&self, path: &str) -> PathBuf {
        self.temp.path().join(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        support::output(
            Command::new(self.path("bin/wi"))
                .args(args)
                .env("HOME", self.path("home"))
                .env("CODEX_HOME", self.path("home/.codex"))
                .env("WI_DATA_DIR", self.path("data"))
                .env("WI_CACHE_DIR", self.path("cache"))
                .env("XDG_CONFIG_HOME", self.path("home/.config"))
                .env("XDG_DATA_HOME", self.path("home/.local/share"))
                .env("XDG_STATE_HOME", self.path("home/.local/state"))
                .env("WI_LAUNCHCTL", self.path("bin/launchctl"))
                .env("WI_SYSTEMCTL", self.path("bin/systemctl"))
                .env_remove("WI_VAULT")
                .env_remove("WI_FAULT"),
        )
    }
    fn ok(&self, args: &[&str]) -> Output {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        out
    }
    fn setup(&self, vault: &str, name: Option<&str>) -> Value {
        let root = self.path(vault);
        let mut args = vec!["setup", "--vault", root.to_str().unwrap(), "--json"];
        if let Some(name) = name {
            args.extend(["--name", name]);
        }
        serde_json::from_slice(&self.ok(&args).stdout).unwrap()
    }
    fn uninstall(&self, vault: &str) {
        let root = self.path(vault);
        self.ok(&["uninstall", "--vault", root.to_str().unwrap()]);
    }
    fn config(&self, host: HostId) -> PathBuf {
        host.config_path(&self.path("home"), Some(&self.path("home/.codex")))
    }
    /// Every wiki-island entry in `host`, with the vault each one serves.
    fn entries(&self, host: HostId) -> Vec<(String, Option<PathBuf>)> {
        let mut entries = wi_export::install::entries(host, &self.config(host));
        entries.sort();
        entries
    }
    fn canonical(&self, vault: &str) -> PathBuf {
        fs::canonicalize(self.path(vault)).unwrap()
    }
    fn owned_skills(&self, vault: &str) -> usize {
        let id = vault_id(&self.path(vault));
        fs::read_dir(self.path("home/.claude/skills"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| {
                        fs::read_to_string(e.path().join(".wi-owned"))
                            .is_ok_and(|owner| owner.contains(&id))
                    })
                    .count()
            })
            .unwrap_or(0)
    }
}

fn vault_id(root: &Path) -> String {
    wi_note::VaultConfig::parse(&fs::read_to_string(root.join(".wiki-island/config.toml")).unwrap())
        .unwrap()
        .vault_id()
        .unwrap()
        .to_owned()
}

/// A procedure note that becomes one exported skill of its vault.
fn add_procedure(f: &Fixture, vault: &str, id: &str) {
    fs::write(
        f.path(vault).join(format!("{id}.md")),
        format!(
            "---\ntype: memory\nkind: procedure\nstatus: active\nskill_id: {id}\ntitle: {id}\nsummary: Steps for {id}\nclaim_kind: intent\nscope: personal\nentities: []\nsources: []\napplicability: {{conditions: []}}\norigin: {{kind: user}}\n---\nDo {id}.\n"
        ),
    )
    .unwrap();
}

fn entry(name: &str, vault: PathBuf) -> (String, Option<PathBuf>) {
    (name.to_owned(), Some(vault))
}

#[test]
fn setup_second_vault_adds_second_entry() {
    let f = Fixture::new();
    let first = f.setup("personal", None);
    assert_eq!(first["entry"], "wiki-island");
    assert!(first["name"].is_null());
    let before: Vec<_> = HOSTS
        .iter()
        .map(|h| fs::read(f.config(*h)).unwrap())
        .collect();
    let second = f.setup("work", Some("work"));
    assert_eq!(second["entry"], "wiki-island-work");
    assert_eq!(second["name"], "work");
    for (host, before) in HOSTS.iter().zip(before) {
        assert_eq!(
            f.entries(*host),
            vec![
                entry("wiki-island", f.canonical("personal")),
                entry("wiki-island-work", f.canonical("work")),
            ],
            "{host:?}"
        );
        let after = fs::read(f.config(*host)).unwrap();
        // The first entry's bytes are unchanged: the second entry is only added.
        let first_entry = first_entry_text(*host, &before);
        assert!(
            String::from_utf8_lossy(&after).contains(&first_entry),
            "{host:?}"
        );
    }
}

/// The text of the default entry as it appears in a host config.
fn first_entry_text(host: HostId, bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    if host == HostId::Codex {
        let start = text.find("[mcp_servers.wiki-island]").unwrap();
        let end = text[start + 1..]
            .find("\n[")
            .map_or(text.len(), |i| start + 1 + i);
        return text[start..end].to_owned();
    }
    let value: Value = serde_json::from_str(&text).unwrap();
    let entry = serde_json::to_string(&value["mcpServers"]["wiki-island"]).unwrap();
    // The JSON writer keeps the entry compact on one line or pretty-printed.
    if text.contains(&entry) {
        entry
    } else {
        let start = text.find("\"wiki-island\"").unwrap();
        let end = text[start..].find('}').unwrap() + start + 1;
        text[start..end].to_owned()
    }
}

#[test]
fn setup_same_vault_updates_entry() {
    let f = Fixture::new();
    f.setup("personal", None);
    let before: Vec<_> = HOSTS
        .iter()
        .map(|h| fs::read(f.config(*h)).unwrap())
        .collect();
    let again = f.setup("personal", None);
    assert_eq!(again["entry"], "wiki-island");
    for (host, before) in HOSTS.iter().zip(before) {
        assert_eq!(
            f.entries(*host),
            vec![entry("wiki-island", f.canonical("personal"))]
        );
        assert_eq!(fs::read(f.config(*host)).unwrap(), before, "{host:?}");
    }
    // A named vault set up again keeps its one entry.
    f.setup("work", Some("work"));
    f.setup("work", None);
    for host in HOSTS {
        assert_eq!(f.entries(host).len(), 2, "{host:?}");
    }
}

#[test]
fn uninstall_leaves_other_vault_entry() {
    for remove_first in [false, true] {
        let f = Fixture::new();
        f.setup("personal", None);
        add_procedure(&f, "personal", "brew-coffee");
        f.setup("personal", None);
        f.setup("work", Some("work"));
        add_procedure(&f, "work", "file-report");
        f.setup("work", None);
        let skills = |vault| f.owned_skills(vault);
        let (personal, work) = (skills("personal"), skills("work"));
        assert!(personal >= 1 && work >= 1, "{personal} {work}");
        let (gone, kept, kept_entry) = if remove_first {
            ("personal", "work", "wiki-island-work")
        } else {
            ("work", "personal", "wiki-island")
        };
        f.uninstall(gone);
        for host in HOSTS {
            assert_eq!(
                f.entries(host),
                vec![entry(kept_entry, f.canonical(kept))],
                "{host:?}"
            );
        }
        assert_eq!(skills(gone), 0);
        assert_eq!(skills(kept), if remove_first { work } else { personal });
        // Notes stay in the disconnected vault.
        assert!(f.path("personal/brew-coffee.md").is_file());
        assert!(f.path("work/file-report.md").is_file());
    }
}

#[test]
fn renamed_vault_leaves_no_stale_entry() {
    let f = Fixture::new();
    f.setup("personal", None);
    f.setup("work", Some("work"));
    let renamed = f.setup("work", Some("job"));
    assert_eq!(renamed["entry"], "wiki-island-job");
    for host in HOSTS {
        assert_eq!(
            f.entries(host),
            vec![
                entry("wiki-island", f.canonical("personal")),
                entry("wiki-island-job", f.canonical("work")),
            ],
            "{host:?}"
        );
    }
    f.uninstall("work");
    for host in HOSTS {
        assert_eq!(
            f.entries(host),
            vec![entry("wiki-island", f.canonical("personal"))]
        );
    }
}

#[test]
fn unnamed_second_vault_gets_an_automatic_name() {
    let f = Fixture::new();
    f.setup("personal", None);
    let out = f.ok(&["setup", "--vault", f.path("Team Notes").to_str().unwrap()]);
    let human = String::from_utf8(out.stdout).unwrap();
    assert!(
        human.contains("AI app(s) as wiki-island-teamnotes."),
        "{human}"
    );
    for host in HOSTS {
        assert_eq!(f.entries(host).len(), 2, "{host:?}");
    }
}

#[test]
fn invalid_name_is_refused_with_a_plain_sentence() {
    let f = Fixture::new();
    let out = f.run(&[
        "setup",
        "--vault",
        f.path("work").to_str().unwrap(),
        "--name",
        "my notes",
    ]);
    assert!(!out.status.success());
    let error = String::from_utf8(out.stderr).unwrap();
    assert!(
        error.contains("The vault name must be 1 to 32 letters, digits, hyphens or underscores"),
        "{error}"
    );
    assert!(!f.path("work").exists());
    assert!(!f.config(HostId::Claude).exists());
}

/// Runs `wi mcp` for `vault`, sends the session-start requests and returns
/// each result with its wire size, measured as in `tests/mcp.rs`.
fn session_start(f: &Fixture, vault: &str) -> Vec<(Value, usize)> {
    use std::io::{BufRead, BufReader, Write};
    let mut child = support::spawn(
        Command::new(f.path("bin/wi"))
            .args(["mcp", "--vault", f.path(vault).to_str().unwrap()])
            .env("HOME", f.path("home"))
            .env("WI_DATA_DIR", f.path("data"))
            .env("WI_CACHE_DIR", f.path("cache"))
            .env_remove("WI_VAULT")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
    );
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut send = |message: Value| {
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
    };
    let mut request = |id: u64, method: &str, params: Value| {
        if id == 2 {
            send(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        }
        send(serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        loop {
            let mut line = String::new();
            assert!(output.read_line(&mut line).unwrap() > 0, "server closed");
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply["id"] == id {
                let result = reply["result"].clone();
                let wire = serde_json::json!({"jsonrpc":"2.0","id":id,"result":result});
                return (result, serde_json::to_vec(&wire).unwrap().len());
            }
        }
    };
    let init_params = serde_json::json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}});
    let brief = serde_json::json!({"uri":"wiki-island://brief"});
    let replies = vec![
        request(1, "initialize", init_params),
        request(2, "tools/list", serde_json::json!({})),
        request(3, "resources/list", serde_json::json!({})),
        request(4, "resources/read", brief),
    ];
    let _ = child.kill();
    let _ = child.wait();
    replies
}

const NAMED_SENTENCE: &str = "Use it only when the conversation is about that subject; another wiki-island vault may be connected for everything else.";

#[test]
fn named_vault_instructions_name_the_vault() {
    let f = Fixture::new();
    f.setup("personal", None);
    let name = "a".repeat(32);
    f.setup("work", Some(&name));
    let unnamed = session_start(&f, "personal");
    let unnamed_text = unnamed[0].0["instructions"].as_str().unwrap().to_owned();
    assert!(unnamed_text.starts_with("Save what you learn as a plain note"));
    assert!(!unnamed_text.contains("This vault is named"));
    let named = session_start(&f, "work");
    let named_text = named[0].0["instructions"].as_str().unwrap();
    assert_eq!(
        named_text,
        format!("{unnamed_text} This vault is named \"{name}\". {NAMED_SENTENCE}")
    );
    assert_eq!(named[0].0["serverInfo"]["name"], "wiki-island");
}

#[test]
fn named_vault_session_start_stays_within_budget() {
    // The vault of the session-start budget test in `tests/mcp.rs`, with a 32-character name.
    let f = Fixture::new();
    let root = f.path("vault");
    f.ok(&["init", root.to_str().unwrap()]);
    for (path, text) in [
        ("coffee.md", "# Coffee\nCoffee is served warm.\n"),
        ("tea.md", "# Tea\nTea is served warm.\n"),
    ] {
        fs::write(root.join(path), text).unwrap();
    }
    let root = fs::canonicalize(&root).unwrap();
    let naming = wi_export::local::Naming {
        name: Some("a".repeat(32)),
        entry: format!("wiki-island-{}", "a".repeat(32)),
    };
    wi_export::local::Targets {
        home: f.path("home"),
        hosts: vec![],
        skill_dirs: vec![],
        cache_root: f.path("cache"),
    }
    .save_named(&root, &naming)
    .unwrap();
    let replies = session_start(&f, "vault");
    assert!(
        replies[0].0["instructions"]
            .as_str()
            .unwrap()
            .ends_with(NAMED_SENTENCE)
    );
    let sizes: Vec<_> = replies.iter().map(|(_, size)| *size).collect();
    let start: usize = sizes.iter().sum();
    eprintln!(
        "Named session wire bytes initialize/tools/resources/brief: {sizes:?}; total {start}"
    );
    assert!(
        start < 8 * 1024,
        "session start uses {start} bytes: {sizes:?}"
    );
}
