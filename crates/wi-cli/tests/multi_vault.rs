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
        // Service fakes: launchd on macOS, a systemd user manager on Linux. Like the real
        // managers, each keeps its state per service in `$HOME/manager`, so one vault's
        // service never looks loaded, enabled or running to another vault.
        f.script(
            "bin/launchctl",
            concat!(
                "#!/bin/sh\nstate=\"$HOME/manager\"\nmkdir -p \"$state\"\ncase $1 in\n",
                "print) test -f \"$state/${2##*/}.loaded\";;\n",
                "bootstrap) touch \"$state/$(basename \"$3\" .plist).loaded\";;\n",
                "bootout) rm -f \"$state/${2##*/}.loaded\";;\n",
                "esac\n",
            ),
        );
        f.script(
            "bin/systemctl",
            concat!(
                "#!/bin/sh\nshift\nstate=\"$HOME/manager\"\nmkdir -p \"$state\"\n",
                "eval \"unit=\\${$#}\"\ncase $1 in\n",
                "show-environment) exit 0;;\n",
                "is-enabled) if test -f \"$state/$unit.enabled\"; then echo enabled; else echo disabled; exit 1; fi; exit 0;;\n",
                "is-active) if test -f \"$state/$unit.active\"; then echo active; else echo inactive; exit 3; fi; exit 0;;\n",
                "enable) touch \"$state/$unit.enabled\";;\n",
                "disable) rm -f \"$state/$unit.enabled\"; test \"$2\" != --now || rm -f \"$state/$unit.active\";;\n",
                "start|restart) touch \"$state/$unit.active\";;\n",
                "stop) rm -f \"$state/$unit.active\";;\n",
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
        self.run_codex(args, &self.path("home/.codex"))
    }
    /// Runs `wi` with the Codex config folder at `codex_home`.
    fn run_codex(&self, args: &[&str], codex_home: &Path) -> Output {
        support::output(
            Command::new(self.path("bin/wi"))
                .args(args)
                .env("HOME", self.path("home"))
                .env("CODEX_HOME", codex_home)
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
    /// True when the fake service manager holds the vault `id`'s own service loaded or running.
    fn service_running(&self, id: &str) -> bool {
        let state = |name: String| self.path("home/manager").join(name).is_file();
        if cfg!(target_os = "macos") {
            state(format!("com.wiki-island.watch.{id}.loaded"))
        } else {
            let unit = format!("wiki-island-{id}.service");
            state(format!("{unit}.enabled")) && state(format!("{unit}.active"))
        }
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
    // Each vault runs its own service; the second does not replace the first.
    for (vault, out) in [("personal", &first), ("work", &second)] {
        assert_eq!(out["background"], true, "{vault}");
        assert!(f.service_running(&vault_id(&f.path(vault))), "{vault}");
    }
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
    entry_text(host, bytes, "wiki-island")
}

/// The text of entry `name` as it appears in a host config.
fn entry_text(host: HostId, bytes: &[u8], name: &str) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    if host == HostId::Codex {
        let start = text.find(&format!("[mcp_servers.{name}]")).unwrap();
        let end = text[start + 1..]
            .find("\n[")
            .map_or(text.len(), |i| start + 1 + i);
        return text[start..end].to_owned();
    }
    let value: Value = serde_json::from_str(&text).unwrap();
    let entry = serde_json::to_string(&value["mcpServers"][name]).unwrap();
    // The JSON writer keeps the entry compact on one line or pretty-printed.
    if text.contains(&entry) {
        entry
    } else {
        let start = text.find(&format!("\"{name}\"")).unwrap();
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
        let (gone_id, kept_id) = (vault_id(&f.path(gone)), vault_id(&f.path(kept)));
        assert!(f.service_running(&gone_id) && f.service_running(&kept_id));
        f.uninstall(gone);
        // Only the removed vault's service stops.
        assert!(!f.service_running(&gone_id));
        assert!(f.service_running(&kept_id));
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

const NAMED_SENTENCE: &str = "Use it only for that subject.";

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

/// Session-start bytes of a vault as `wi setup` leaves it, with the two notes of the
/// budget test in `tests/mcp.rs`.
fn session_start_after_setup(f: &Fixture, vault: &str, name: Option<&str>) -> usize {
    f.setup(vault, name);
    let root = f.path(vault);
    for (path, text) in [
        ("coffee.md", "# Coffee\nCoffee is served warm.\n"),
        ("tea.md", "# Tea\nTea is served warm.\n"),
    ] {
        fs::write(root.join(path), text).unwrap();
    }
    let sizes: Vec<_> = session_start(f, vault)
        .iter()
        .map(|(_, size)| *size)
        .collect();
    eprintln!("After-setup session start {vault}: {sizes:?}");
    sizes.iter().sum()
}

#[test]
fn session_start_after_setup_stays_within_budget() {
    let f = Fixture::new();
    let unnamed = session_start_after_setup(&f, "plain", None);
    assert!(unnamed <= 8192 - 88, "session start uses {unnamed} bytes");
    let name = "a".repeat(32);
    let named = session_start_after_setup(&f, "work", Some(&name));
    // ` This vault is named "<name>". Use it only for that subject.`: 54 bytes plus the name,
    // plus two backslashes that JSON puts before the quotes on the wire.
    assert_eq!(named, unnamed + 56 + name.len());
    assert!(named <= 8192, "named session start uses {named} bytes");
}

#[test]
fn doctor_shows_the_vault_name_and_entry() {
    let f = Fixture::new();
    f.setup("personal", None);
    f.setup("work", Some("work"));
    let doctor = |vault: &str| {
        let root = f.path(vault);
        let out = f.ok(&["doctor", "--vault", root.to_str().unwrap()]);
        String::from_utf8(out.stdout).unwrap()
    };
    let personal = doctor("personal");
    assert!(
        personal.contains("[ok] apps: This is the default vault; AI apps reach it as wiki-island."),
        "{personal}"
    );
    let work = doctor("work");
    assert!(
        work.contains(
            "[ok] apps: This vault is named \"work\"; AI apps reach it as wiki-island-work."
        ),
        "{work}"
    );
    // An app whose config lost this vault's entry is listed with the next step.
    let claude = f.config(HostId::Claude);
    let mut config: Value = serde_json::from_slice(&fs::read(&claude).unwrap()).unwrap();
    config["mcpServers"]
        .as_object_mut()
        .unwrap()
        .remove("wiki-island-work");
    fs::write(&claude, serde_json::to_vec(&config).unwrap()).unwrap();
    let work = doctor("work");
    assert!(
        work.contains("[warn] apps: This vault is named \"work\"; AI apps reach it as wiki-island-work. These apps no longer have it: Claude Code."),
        "{work}"
    );
    assert!(work.contains("Next: run wi setup --vault"), "{work}");
    let out = f.ok(&[
        "doctor",
        "--json",
        "--vault",
        f.path("work").to_str().unwrap(),
    ]);
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    let check = json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "apps")
        .unwrap()
        .clone();
    assert_eq!(check["status"], "warn");
}

#[test]
fn mcp_install_rename_migrates_recorded_hosts() {
    let f = Fixture::new();
    f.setup("first", None);
    f.setup("second", Some("second"));
    let (first, second) = (f.canonical("first"), f.canonical("second"));
    // Claude's default entry now serves the second vault, so the first vault needs a new name.
    let claude = f.config(HostId::Claude);
    let text = fs::read_to_string(&claude).unwrap();
    let edited = text.replace(first.to_str().unwrap(), second.to_str().unwrap());
    assert_ne!(text, edited);
    fs::write(&claude, edited).unwrap();
    let second_entries = |host: HostId| -> Vec<String> {
        let bytes = fs::read(f.config(host)).unwrap();
        f.entries(host)
            .into_iter()
            .filter(|(_, vault)| vault.as_deref() == Some(second.as_path()))
            .map(|(name, _)| entry_text(host, &bytes, &name))
            .collect()
    };
    let before: Vec<_> = HOSTS.iter().map(|h| second_entries(*h)).collect();
    f.ok(&[
        "mcp",
        "install",
        "--host",
        "claude",
        "--yes",
        "--vault",
        f.path("first").to_str().unwrap(),
    ]);
    for (host, before) in HOSTS.iter().zip(before) {
        let mine: Vec<_> = f
            .entries(*host)
            .into_iter()
            .filter(|(_, vault)| vault.as_deref() == Some(first.as_path()))
            .collect();
        assert_eq!(
            mine,
            vec![entry("wiki-island-first", first.clone())],
            "{host:?}"
        );
        let after = String::from_utf8(fs::read(f.config(*host)).unwrap()).unwrap();
        for text in before {
            assert!(after.contains(&text), "{host:?}: {text}");
        }
    }
    f.uninstall("first");
    for host in HOSTS {
        assert!(
            f.entries(host)
                .iter()
                .all(|(_, vault)| vault.as_deref() != Some(first.as_path())),
            "{host:?}: {:?}",
            f.entries(host)
        );
    }
}

#[test]
fn mcp_install_records_its_entry_name() {
    let f = Fixture::new();
    f.setup("first", None);
    let second = f.path("second");
    f.ok(&["init", second.to_str().unwrap()]);
    let install = || {
        f.ok(&[
            "mcp",
            "install",
            "--host",
            "claude",
            "--yes",
            "--vault",
            second.to_str().unwrap(),
        ])
    };
    install();
    f.uninstall("first");
    install();
    assert_eq!(
        f.entries(HostId::Claude),
        vec![entry("wiki-island-second", f.canonical("second"))]
    );
    f.setup("second", None);
    f.uninstall("second");
    for host in HOSTS {
        assert_eq!(f.entries(host), vec![], "{host:?}");
    }
}

/// The text of every file in `paths`; `None` for a missing file.
fn snapshot(paths: &[PathBuf]) -> Vec<(PathBuf, Option<String>)> {
    paths
        .iter()
        .map(|path| {
            let bytes = fs::read(path).ok();
            (path.clone(), bytes.map(|b| String::from_utf8(b).unwrap()))
        })
        .collect()
}

/// Every app config of the fixture and the device record of each vault.
fn touched_files(f: &Fixture, vaults: &[&str]) -> Vec<PathBuf> {
    HOSTS
        .iter()
        .map(|host| f.config(*host))
        .chain(
            vaults
                .iter()
                .map(|vault| f.path(vault).join(".wiki-island/local/targets.json")),
        )
        .collect()
}

#[test]
fn mcp_install_after_config_folder_change_keeps_other_vault() {
    let f = Fixture::new();
    let (old_codex, new_codex) = (f.path("home/.codex"), f.path("codex-new"));
    fs::create_dir_all(&new_codex).unwrap();
    let install = |vault: &str, codex_home: &Path| {
        let root = f.path(vault);
        let args = [
            "mcp",
            "install",
            "--host",
            "codex",
            "--yes",
            "--vault",
            root.to_str().unwrap(),
        ];
        let out = f.run_codex(&args, codex_home);
        assert!(out.status.success(), "{args:?}: {out:?}");
    };
    f.ok(&["init", f.path("first").to_str().unwrap()]);
    install("first", &old_codex);
    f.ok(&["init", f.path("second").to_str().unwrap()]);
    install("second", &new_codex);
    let (first, second) = (f.canonical("first"), f.canonical("second"));
    let new_config = new_codex.join("config.toml");
    let old_config = old_codex.join("config.toml");
    let entries = |path: &Path| {
        let mut entries = wi_export::install::entries(HostId::Codex, path);
        entries.sort();
        entries
    };
    assert_eq!(
        entries(&new_config),
        vec![entry("wiki-island", second.clone())]
    );
    let second_text = entry_text(
        HostId::Codex,
        &fs::read(&new_config).unwrap(),
        "wiki-island",
    );
    install("first", &new_codex);
    let after = fs::read(&new_config).unwrap();
    assert_eq!(
        entry_text(HostId::Codex, &after, "wiki-island"),
        second_text
    );
    let mine: Vec<_> = entries(&new_config)
        .into_iter()
        .filter(|(_, vault)| vault.as_deref() == Some(first.as_path()))
        .collect();
    assert_eq!(mine.len(), 1, "{:?}", entries(&new_config));
    assert_ne!(mine[0].0, "wiki-island");
    let out = f.run_codex(
        &["uninstall", "--vault", f.path("first").to_str().unwrap()],
        &new_codex,
    );
    assert!(out.status.success(), "{out:?}");
    for path in [&old_config, &new_config, &f.config(HostId::Claude)] {
        assert!(
            entries(path)
                .iter()
                .all(|(_, vault)| vault.as_deref() != Some(first.as_path())),
            "{path:?}: {:?}",
            entries(path)
        );
    }
    assert_eq!(entries(&new_config), vec![entry("wiki-island", second)]);
}

#[test]
fn failed_rename_changes_nothing() {
    let f = Fixture::new();
    f.setup("first", None);
    f.setup("second", Some("second"));
    let (first, second) = (f.canonical("first"), f.canonical("second"));
    // Cursor's default entry now serves the second vault, so the first vault must rename.
    let cursor = f.config(HostId::Cursor);
    let text = fs::read_to_string(&cursor).unwrap();
    fs::write(
        &cursor,
        text.replace(first.to_str().unwrap(), second.to_str().unwrap()),
    )
    .unwrap();
    fs::write(f.config(HostId::Codex), "mcp_servers = [").unwrap();
    let files = touched_files(&f, &["first", "second"]);
    let before = snapshot(&files);
    let out = f.run(&[
        "mcp",
        "install",
        "--host",
        "claude",
        "--yes",
        "--vault",
        f.path("first").to_str().unwrap(),
    ]);
    assert!(!out.status.success(), "{out:?}");
    let error = String::from_utf8(out.stderr).unwrap();
    assert!(error.contains("settings"), "{error}");
    assert_eq!(snapshot(&files), before);
}

#[test]
fn failed_setup_rename_changes_nothing() {
    let f = Fixture::new();
    f.setup("first", None);
    f.setup("second", Some("second"));
    fs::write(f.config(HostId::Codex), "mcp_servers = [").unwrap();
    let files = touched_files(&f, &["first", "second"]);
    let before = snapshot(&files);
    let out = f.run(&[
        "setup",
        "--vault",
        f.path("first").to_str().unwrap(),
        "--name",
        "other",
    ]);
    assert!(!out.status.success(), "{out:?}");
    let error = String::from_utf8(out.stderr).unwrap();
    assert!(error.contains("Setup could not"), "{error}");
    assert_eq!(snapshot(&files), before);
}

/// Replace the first vault's device record with bytes no version can read.
fn damage_record(f: &Fixture, vault: &str) {
    fs::write(
        f.path(vault).join(".wiki-island/local/targets.json"),
        "{broken",
    )
    .unwrap();
}

#[test]
fn setup_with_a_damaged_record_changes_nothing() {
    let f = Fixture::new();
    f.setup("first", None);
    f.setup("second", Some("second"));
    damage_record(&f, "first");
    let files = touched_files(&f, &["first", "second"]);
    let before = snapshot(&files);
    let out = f.run(&[
        "setup",
        "--vault",
        f.path("first").to_str().unwrap(),
        "--name",
        "new",
    ]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let error = String::from_utf8(out.stderr).unwrap();
    assert!(
        error.contains("Setup could not finish; check that wiki-island is installed in its permanent location and your AI app settings are readable and writable, then run wi setup again."),
        "{error}"
    );
    assert_eq!(snapshot(&files), before);
}

#[test]
fn mcp_install_with_a_damaged_record_changes_nothing() {
    let f = Fixture::new();
    f.setup("first", None);
    f.setup("second", Some("second"));
    damage_record(&f, "first");
    let files = touched_files(&f, &["first", "second"]);
    let before = snapshot(&files);
    let out = f.run(&[
        "mcp",
        "install",
        "--host",
        "claude",
        "--yes",
        "--vault",
        f.path("first").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let error = String::from_utf8(out.stderr).unwrap();
    assert!(
        error.contains("Installation destinations could not be read."),
        "{error}"
    );
    assert_eq!(snapshot(&files), before);
}

/// Add an entry named `name`, outside the naming rule, that serves `vault`.
fn add_foreign_entry(f: &Fixture, host: HostId, name: &str, vault: &Path) {
    let path = f.config(host);
    let text = fs::read_to_string(&path).unwrap();
    let binary = f.path("bin/wi");
    let edited = if host == HostId::Codex {
        format!(
            "{text}\n[mcp_servers.{name}]\ncommand = \"{}\"\nargs = [\"mcp\", \"--vault\", \"{}\"]\n",
            binary.display(),
            vault.display()
        )
    } else {
        let mut value: Value = serde_json::from_str(&text).unwrap();
        value["mcpServers"][name] = serde_json::json!({
            "command": binary,
            "args": ["mcp", "--vault", vault],
        });
        serde_json::to_string_pretty(&value).unwrap()
    };
    fs::write(&path, edited).unwrap();
}

#[test]
fn recorded_entry_outside_the_naming_rule_is_resolved_again() {
    for host in [HostId::Claude, HostId::Codex] {
        let f = Fixture::new();
        f.setup("first", None);
        f.setup("second", Some("second"));
        let (first, second) = (f.canonical("first"), f.canonical("second"));
        add_foreign_entry(&f, host, "owner-entry", &second);
        let record = f.path("first").join(".wiki-island/local/targets.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        value["entry"] = "owner-entry".into();
        fs::write(&record, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let owner = entry_text(host, &fs::read(f.config(host)).unwrap(), "owner-entry");
        let host_name = serde_json::to_value(host).unwrap();
        f.ok(&[
            "mcp",
            "install",
            "--host",
            host_name.as_str().unwrap(),
            "--yes",
            "--vault",
            f.path("first").to_str().unwrap(),
        ]);
        let after = fs::read(f.config(host)).unwrap();
        assert_eq!(entry_text(host, &after, "owner-entry"), owner, "{host:?}");
        let mine: Vec<_> = f
            .entries(host)
            .into_iter()
            .filter(|(_, vault)| vault.as_deref() == Some(first.as_path()))
            .collect();
        assert_eq!(mine, vec![entry("wiki-island", first.clone())], "{host:?}");
        f.uninstall("first");
        let after = fs::read(f.config(host)).unwrap();
        assert_eq!(entry_text(host, &after, "owner-entry"), owner, "{host:?}");
        assert!(
            f.entries(host)
                .iter()
                .all(|(_, vault)| vault.as_deref() != Some(first.as_path())),
            "{host:?}"
        );
    }
}

/// Set a vault up and remove it again, so its id is known and no service or record remains.
#[cfg(target_os = "linux")]
fn vault_without_service(f: &Fixture) -> String {
    f.setup("first", None);
    let id = vault_id(&f.path("first"));
    f.uninstall("first");
    id
}

#[cfg(target_os = "linux")]
fn setup_first(f: &Fixture) -> (String, String) {
    let out = f.run(&["setup", "--vault", f.path("first").to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    (
        String::from_utf8(out.stderr).unwrap(),
        String::from_utf8(out.stdout).unwrap(),
    )
}

#[cfg(target_os = "linux")]
#[test]
fn setup_foreign_unit_reports_stop_step() {
    let f = Fixture::new();
    let id = vault_without_service(&f);
    // A unit of this vault's name runs, but no unit file wiki-island knows exists.
    fs::write(
        f.path("home/manager")
            .join(format!("wiki-island-{id}.service.active")),
        "",
    )
    .unwrap();
    let (error, stdout) = setup_first(&f);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        error.contains(
            "is running without a unit file wiki-island knows; stop it, then run wi setup again"
        ),
        "{error}"
    );
    assert!(!error.contains("Setup could not"), "{error}");
}

#[cfg(target_os = "linux")]
#[test]
fn setup_foreign_unit_file_reports_rename_step() {
    let f = Fixture::new();
    let id = vault_without_service(&f);
    let unit = f
        .path("home/.config/systemd/user")
        .join(format!("wiki-island-{id}.service"));
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    fs::write(&unit, "[Service]\nExecStart=/bin/true\n").unwrap();
    let (error, _) = setup_first(&f);
    assert!(
        error.contains("already exists; rename or remove it, then run wi setup again"),
        "{error}"
    );
    assert!(!error.contains("Setup could not"), "{error}");
    assert_eq!(
        fs::read_to_string(&unit).unwrap(),
        "[Service]\nExecStart=/bin/true\n"
    );
}

#[test]
fn doctor_reports_resolved_entry_name() {
    let f = Fixture::new();
    f.setup("first", None);
    f.setup("second", Some("second"));
    let doctor = |vault: &str| {
        let out = f.ok(&["doctor", "--vault", f.path(vault).to_str().unwrap()]);
        String::from_utf8(out.stdout).unwrap()
    };
    // A supported recorded name is printed as before.
    assert!(
        doctor("second").contains("AI apps reach it as wiki-island-second."),
        "{}",
        doctor("second")
    );
    let record = f.path("first").join(".wiki-island/local/targets.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    value["entry"] = "owner entry!".into();
    fs::write(&record, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let text = doctor("first");
    assert!(!text.contains("owner entry!"), "{text}");
    let line = text.lines().find(|l| l.contains("apps:")).unwrap();
    assert!(line.starts_with("[warn] apps:"), "{text}");
    assert!(text.contains("Next: run wi setup"), "{text}");
}
