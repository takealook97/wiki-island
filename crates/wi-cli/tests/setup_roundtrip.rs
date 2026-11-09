#![cfg(unix)]
mod support;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
}
impl Fixture {
    fn new() -> Self {
        let f = Self {
            temp: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(f.path("home")).unwrap();
        fs::create_dir_all(f.path("bin")).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_wi"), f.path("bin/wi")).unwrap();
        let fake = f.path("bin/launchctl");
        fs::write(&fake, "#!/bin/sh\necho \"$1\" >> \"$HOME/calls\"\ncase $1 in\nprint) test -f \"$HOME/loaded\";;\nbootstrap) touch \"$HOME/loaded\";;\nbootout) rm -f \"$HOME/loaded\";;\nesac\n").unwrap();
        fs::set_permissions(fake, fs::Permissions::from_mode(0o755)).unwrap();
        // A user manager that logs every change; `no-manager` and `fail-start` in HOME
        // simulate a missing session and a watcher that cannot start.
        let fake = f.path("bin/systemctl");
        fs::write(&fake, concat!(
            "#!/bin/sh\nshift\ncase $1 in\n",
            "show-environment) test ! -f \"$HOME/no-manager\"; exit $?;;\n",
            "is-enabled) if test -f \"$HOME/enabled\"; then echo enabled; else echo disabled; exit 1; fi; exit 0;;\n",
            "is-active) if test -f \"$HOME/active\"; then echo active; else echo inactive; exit 3; fi; exit 0;;\n",
            "esac\necho \"$*\" >> \"$HOME/calls\"\ncase $1 in\n",
            "enable) touch \"$HOME/enabled\";;\n",
            "disable) rm -f \"$HOME/enabled\"; test \"$2\" != --now || rm -f \"$HOME/active\";;\n",
            "start|restart) test ! -f \"$HOME/fail-start\" || exit 1; touch \"$HOME/active\";;\n",
            "stop) rm -f \"$HOME/active\";;\n",
            "esac\nexit 0\n",
        )).unwrap();
        fs::set_permissions(fake, fs::Permissions::from_mode(0o755)).unwrap();
        f
    }
    fn vault_id(&self) -> String {
        wi_note::VaultConfig::parse(
            &fs::read_to_string(self.path("vault/.wiki-island/config.toml")).unwrap(),
        )
        .unwrap()
        .vault_id()
        .unwrap()
        .to_owned()
    }
    fn unit(&self, id: &str) -> std::path::PathBuf {
        self.path(&format!(
            "home/.config/systemd/user/wiki-island-{id}.service"
        ))
    }
    fn path(&self, path: &str) -> std::path::PathBuf {
        self.temp.path().join(path)
    }
    fn run_binary(&self, binary: &Path, args: &[&str]) -> Output {
        support::output(
            Command::new(binary)
                .args(args)
                .env("HOME", self.path("home"))
                .env("USERPROFILE", self.path("home"))
                .env("APPDATA", self.path("home"))
                .env("LOCALAPPDATA", self.path("home"))
                .env("CODEX_HOME", self.path("home/.codex"))
                .env("WI_VAULT", self.path("vault"))
                .env("WI_CACHE_DIR", self.path("cache"))
                .env("WI_LAUNCHCTL", self.path("bin/launchctl"))
                .env("WI_SYSTEMCTL", self.path("bin/systemctl"))
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("WI_FAULT"),
        )
    }
    fn run(&self, args: &[&str]) -> Output {
        self.run_binary(&self.path("bin/wi"), args)
    }
    fn seed(&self, path: &str, bytes: &str) {
        let path = self.path(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn assert_setup_without_service(&self, out: &Output) {
        assert!(out.status.success(), "{out:?}");
        assert!(self.path("vault/.git").is_dir());
        assert!(
            fs::read_to_string(self.path("home/.claude.json"))
                .unwrap()
                .contains("wiki-island")
        );
        assert!(!self.path("home/calls").exists());
        assert!(!self.path("home/loaded").exists());
    }
}
/// Targets with a service backend: launchd on macOS, systemd on Linux.
const SERVICE: bool = cfg!(any(target_os = "macos", target_os = "linux"));
const LINUX: bool = cfg!(target_os = "linux");

#[test]
fn setup_preflights_development_binary_without_changing_home_or_vault() {
    let f = Fixture::new();
    f.seed("home/.claude.json", "{\"theme\":1}\n");
    let out = f.run_binary(Path::new(env!("CARGO_BIN_EXE_wi")), &["setup"]);
    if !SERVICE {
        f.assert_setup_without_service(&out);
        assert!(!f.path("home/Library/LaunchAgents").exists());
        return;
    }
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(f.path("home/.claude.json")).unwrap(),
        "{\"theme\":1}\n"
    );
    assert!(!f.path("vault").exists());
    let error = String::from_utf8(out.stderr).unwrap();
    assert!(!error.contains("--allow-dev-binary"), "{error}");
    assert!(!f.path("home/calls").exists());
}
#[test]
fn setup_preflights_all_host_configs_before_writing_anything() {
    let f = Fixture::new();
    f.seed("home/.claude.json", "{\"theme\":1}\n");
    f.seed("home/.codex/config.toml", "broken = [");
    let out = f.run(&["setup"]);
    assert!(!out.status.success());
    assert_eq!(
        fs::read_to_string(f.path("home/.claude.json")).unwrap(),
        "{\"theme\":1}\n"
    );
    assert!(!f.path("vault").exists());
    assert!(!f.path("home/calls").exists());
}
#[test]
fn setup_twice_and_uninstall_restore_exact_host_bytes_and_created_containers() {
    let f = Fixture::new();
    let seeds = [
        ("home/.claude.json", "{\"theme\":1}\n"),
        ("home/.codex/config.toml", "# keep\nmodel = 'chosen'\n"),
        (
            "home/.cursor/mcp.json",
            "{\"mcpServers\":{\"other\":{}, // keep trailing\n}}\n",
        ),
        ("home/.gemini/settings.json", "{\"theme\":\"dark\"}\n"),
    ];
    for (path, bytes) in seeds {
        f.seed(path, bytes);
    }
    fs::create_dir_all(f.path("home/Library/Application Support/Code/User")).unwrap();
    let setup = f.run(&["setup", "--json"]);
    assert!(setup.status.success(), "{setup:?}");
    if cfg!(target_os = "macos") {
        assert!(
            fs::read_to_string(f.path("home/calls"))
                .unwrap()
                .contains("bootstrap")
        );
    } else if LINUX {
        let calls = fs::read_to_string(f.path("home/calls")).unwrap();
        assert!(calls.contains("enable wiki-island-"), "{calls}");
        assert!(f.path("home/active").exists());
    } else {
        assert!(!f.path("home/calls").exists());
    }
    let configured: Vec<_> = seeds
        .iter()
        .map(|(p, _)| fs::read(f.path(p)).unwrap())
        .collect();
    let second = f.run(&["setup", "--json"]);
    assert!(second.status.success(), "{second:?}");
    for ((path, _), bytes) in seeds.iter().zip(configured) {
        assert_eq!(fs::read(f.path(path)).unwrap(), bytes);
    }
    if cfg!(target_os = "macos") {
        assert_eq!(
            fs::read_to_string(f.path("home/calls"))
                .unwrap()
                .matches("bootstrap")
                .count(),
            1
        );
    } else if LINUX {
        let calls = fs::read_to_string(f.path("home/calls")).unwrap();
        assert_eq!(
            calls.lines().filter(|l| l.starts_with("restart")).count(),
            1,
            "{calls}"
        );
    } else {
        assert!(!f.path("home/calls").exists());
    }
    let out = f.run(&["uninstall"]);
    assert!(out.status.success(), "{out:?}");
    for (path, bytes) in seeds {
        assert_eq!(fs::read_to_string(f.path(path)).unwrap(), bytes, "{path}");
    }
    for path in [
        "home/Library/Application Support/Code/User/mcp.json",
        "home/.claude/skills",
        "home/.agents/skills",
        "home/Library/LaunchAgents",
        "home/Library/Logs",
        "home/.config/systemd",
    ] {
        assert!(!f.path(path).exists(), "left behind: {path}");
    }
    assert!(f.path("vault/.git").is_dir());
    fn no_backups(path: &Path) {
        for entry in fs::read_dir(path).unwrap().flatten() {
            assert!(!entry.file_name().to_string_lossy().contains("wi-backup"));
            if entry.file_type().unwrap().is_dir() {
                no_backups(&entry.path());
            }
        }
    }
    no_backups(&f.path("home"));
}
#[test]
fn registration_preserves_stable_executable_symlink() {
    let f = Fixture::new();
    f.seed("home/.claude.json", "{}\n");
    std::os::unix::fs::symlink(f.path("bin/wi"), f.path("bin/stable-wi")).unwrap();
    let out = f.run_binary(&f.path("bin/stable-wi"), &["setup"]);
    assert!(out.status.success(), "{out:?}");
    let text = fs::read_to_string(f.path("home/.claude.json")).unwrap();
    assert!(
        text.contains(f.path("bin/stable-wi").to_str().unwrap()),
        "{text}"
    );
    if LINUX {
        let unit = fs::read_to_string(f.unit(&f.vault_id())).unwrap();
        assert!(
            unit.contains(&format!(
                "ExecStart=\"{}\"",
                f.path("bin/stable-wi").display()
            )),
            "{unit}"
        );
    }
}

// A single stored ZIP member, without a platform command or test dependency.
fn archive(name: &str, body: &[u8]) -> Vec<u8> {
    fn word(out: &mut Vec<u8>, n: u16) {
        out.extend(n.to_le_bytes());
    }
    fn dword(out: &mut Vec<u8>, n: u32) {
        out.extend(n.to_le_bytes());
    }
    let mut crc = !0u32;
    for byte in body {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    crc = !crc;
    let mut out = Vec::new();
    dword(&mut out, 0x04034b50);
    for n in [20, 0, 0, 0, 0] {
        word(&mut out, n);
    }
    for n in [crc, body.len() as u32, body.len() as u32] {
        dword(&mut out, n);
    }
    word(&mut out, name.len() as u16);
    word(&mut out, 0);
    out.extend(name.as_bytes());
    out.extend(body);
    let offset = out.len() as u32;
    dword(&mut out, 0x02014b50);
    for n in [20, 20, 0, 0, 0, 0] {
        word(&mut out, n);
    }
    for n in [crc, body.len() as u32, body.len() as u32] {
        dword(&mut out, n);
    }
    for n in [name.len() as u16, 0, 0, 0, 0] {
        word(&mut out, n);
    }
    dword(&mut out, 0);
    dword(&mut out, 0);
    out.extend(name.as_bytes());
    let central_size = out.len() as u32 - offset;
    dword(&mut out, 0x06054b50);
    for n in [0, 0, 1, 1] {
        word(&mut out, n);
    }
    dword(&mut out, central_size);
    dword(&mut out, offset);
    word(&mut out, 0);
    out
}
#[test]
fn binary_imports_absolute_zip_and_folder_from_downloads_without_changing_sources() {
    let f = Fixture::new();
    assert!(f.run(&["init"]).status.success());
    let name = "Page 11111111111111111111111111111111.md";
    let body = "---\ntype: memory\nkind: rule\nstatus: active\n---\nImported content.\n";
    f.seed(&format!("home/Downloads/Export/{name}"), body);
    let zip = archive(name, body.as_bytes());
    fs::write(f.path("home/Downloads/Export.zip"), &zip).unwrap();
    for source in ["home/Downloads/Export", "home/Downloads/Export.zip"] {
        let out = f.run(&["import", f.path(source).to_str().unwrap(), "--json"]);
        assert!(out.status.success(), "{out:?}");
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["imports"][0]["status"], "complete", "{value}");
        let output = value["imports"][0]["sidecar_path"].as_str().unwrap();
        let copied = fs::read(f.path(&format!("vault/{output}/Page.md"))).unwrap();
        let note = wi_note::Note::parse(&copied).unwrap();
        assert_eq!(note.frontmatter().unwrap().get("type").unwrap(), "note");
    }
    assert!(f.run(&["maintain"]).status.success());
    assert_eq!(
        fs::read(f.path(&format!("home/Downloads/Export/{name}"))).unwrap(),
        body.as_bytes()
    );
    assert_eq!(fs::read(f.path("home/Downloads/Export.zip")).unwrap(), zip);
}

#[test]
fn uninstall_keeps_preexisting_service_and_skill_containers() {
    let f = Fixture::new();
    f.seed("home/.claude.json", "{}\n");
    for path in [
        "home/Library/LaunchAgents",
        "home/Library/Logs/wiki-island",
        "home/.config/systemd/user",
        "home/.claude/skills",
    ] {
        fs::create_dir_all(f.path(path)).unwrap();
    }
    let out = f.run(&["setup"]);
    assert!(out.status.success(), "{out:?}");
    let id = wi_note::VaultConfig::parse(
        &fs::read_to_string(f.path("vault/.wiki-island/config.toml")).unwrap(),
    )
    .unwrap()
    .vault_id()
    .unwrap()
    .to_owned();
    f.seed(
        &format!("home/Library/Logs/wiki-island/{id}.log"),
        "owned log\n",
    );
    let out = f.run(&["uninstall"]);
    assert!(out.status.success(), "{out:?}");
    let log = f.path(&format!("home/Library/Logs/wiki-island/{id}.log"));
    if cfg!(target_os = "macos") {
        assert!(!log.exists());
    } else {
        assert_eq!(fs::read_to_string(log).unwrap(), "owned log\n");
    }
    for path in [
        "home/Library/LaunchAgents",
        "home/Library/Logs/wiki-island",
        "home/.config/systemd/user",
        "home/.claude/skills",
    ] {
        assert!(f.path(path).is_dir(), "{path}");
    }
    if LINUX {
        assert!(!f.unit(&id).exists());
    }
}

#[test]
fn setup_reports_a_legacy_user_skill_collision_and_keeps_the_user_skill() {
    let f = Fixture::new();
    f.seed("home/.claude.json", "{}\n");
    f.seed(
        "home/.claude/skills/wi-vault-maintenance/SKILL.md",
        "User skill\n",
    );
    let out = f.run(&["setup"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("alternative names"), "{text}");
    assert_eq!(
        fs::read_to_string(f.path("home/.claude/skills/wi-vault-maintenance/SKILL.md")).unwrap(),
        "User skill\n"
    );
    assert_eq!(
        fs::read_dir(f.path("home/.claude/skills")).unwrap().count(),
        2
    );
}

#[test]
fn failed_background_start_does_not_leave_host_connections_or_service_files() {
    let f = Fixture::new();
    f.seed("home/.claude.json", "{\"theme\":1}\n");
    f.seed(
        "bin/launchctl",
        "#!/bin/sh\ncase $1 in bootstrap) exit 1;; print) exit 3;; *) exit 0;; esac\n",
    );
    f.seed("home/fail-start", "");
    let out = f.run(&["setup"]);
    if !SERVICE {
        f.assert_setup_without_service(&out);
        assert!(!f.path("home/Library/LaunchAgents").exists());
        assert!(!f.path("home/Library/Logs").exists());
        return;
    }
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(f.path("home/.claude.json")).unwrap(),
        "{\"theme\":1}\n"
    );
    assert!(!f.path("home/.claude/skills").exists());
    assert!(!f.path("home/Library/LaunchAgents").exists());
    assert!(!f.path("home/Library/Logs").exists());
    assert!(!f.path("home/.config/systemd").exists());
    assert!(!f.path("home/enabled").exists());
    assert!(!f.path("home/active").exists());
    let message = String::from_utf8(out.stderr).unwrap();
    assert!(!message.contains("launchctl"), "{message}");
    assert!(!message.contains("systemctl"), "{message}");
    assert_eq!(message.lines().count(), 1, "{message}");
}

#[test]
fn setup_preflights_the_existing_service_destination() {
    let f = Fixture::new();
    f.seed("home/.claude.json", "{\"theme\":1}\n");
    assert!(f.run(&["init"]).status.success());
    let id = wi_note::VaultConfig::parse(
        &fs::read_to_string(f.path("vault/.wiki-island/config.toml")).unwrap(),
    )
    .unwrap()
    .vault_id()
    .unwrap()
    .to_owned();
    f.seed("foreign.plist", "keep\n");
    fs::create_dir_all(f.path("home/Library/LaunchAgents")).unwrap();
    std::os::unix::fs::symlink(
        f.path("foreign.plist"),
        f.path(&format!(
            "home/Library/LaunchAgents/com.wiki-island.watch.{id}.plist"
        )),
    )
    .unwrap();
    if LINUX {
        fs::create_dir_all(f.unit(&id).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(f.path("foreign.plist"), f.unit(&id)).unwrap();
    }
    let out = f.run(&["setup"]);
    if !SERVICE {
        f.assert_setup_without_service(&out);
        assert_eq!(
            fs::read_to_string(f.path("foreign.plist")).unwrap(),
            "keep\n"
        );
        assert!(
            fs::symlink_metadata(f.path(&format!(
                "home/Library/LaunchAgents/com.wiki-island.watch.{id}.plist"
            )))
            .unwrap()
            .file_type()
            .is_symlink()
        );
        return;
    }
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(f.path("home/.claude.json")).unwrap(),
        "{\"theme\":1}\n"
    );
    assert_eq!(
        fs::read_to_string(f.path("foreign.plist")).unwrap(),
        "keep\n"
    );
    assert!(!f.path("home/calls").exists());
}

#[test]
fn invalid_json_settings_report_one_plain_sentence_without_mutations() {
    let f = Fixture::new();
    f.seed("home/.claude.json", "[]\n");
    let out = f.run(&["setup"]);
    assert!(!out.status.success());
    let message = String::from_utf8(out.stderr).unwrap();
    assert_eq!(message.lines().count(), 1, "{message}");
    assert!(!message.contains("JSON"), "{message}");
    assert_eq!(
        fs::read_to_string(f.path("home/.claude.json")).unwrap(),
        "[]\n"
    );
    assert!(!f.path("vault").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn doctor_checks_loaded_service_and_uninstall_removes_disposable_cache() {
    let f = Fixture::new();
    assert!(f.run(&["setup"]).status.success());
    let before = f.run(&["doctor"]);
    assert!(before.status.success(), "{before:?}");
    let text = String::from_utf8(before.stdout).unwrap();
    assert!(
        text.contains("[ok] service: Background care is running."),
        "{text}"
    );
    for jargon in [
        "manifest",
        "generation",
        "stat-walk",
        "branch tip",
        "complete walk",
    ] {
        assert!(!text.contains(jargon), "{text}");
    }
    fs::remove_file(f.path(if LINUX { "home/active" } else { "home/loaded" })).unwrap();
    let after = f.run(&["doctor"]);
    let text = String::from_utf8(after.stdout).unwrap();
    assert!(
        text.contains("[warn] service: Background care is not running."),
        "{text}"
    );
    assert!(text.contains("run wi setup"));
    assert!(fs::read_dir(f.path("cache")).unwrap().next().is_some());
    let out = f.run(&["uninstall"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(fs::read_dir(f.path("cache")).unwrap().next().is_none());
    assert!(f.path("vault/.git").is_dir());
    if LINUX {
        assert!(!f.path("home/.config/systemd").exists());
        assert!(!f.path("home/enabled").exists());
    }
}

#[test]
fn setup_records_destinations_and_later_home_changes_cannot_redirect_exports_or_removal() {
    let f = Fixture::new();
    f.seed("home/.claude.json", "{\"theme\":1}\n");
    f.seed("home/.codex/config.toml", "# owner setting\n");
    let setup = f.run(&["setup"]);
    assert!(setup.status.success(), "{setup:?}");
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(f.path("vault/.wiki-island/local/targets.json")).unwrap())
            .unwrap();
    assert_eq!(state["home"], f.path("home").to_str().unwrap());
    assert!(
        state["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| Path::new(entry[1].as_str().unwrap()).is_absolute())
    );
    if cfg!(target_os = "macos") {
        let plist = fs::read_dir(f.path("home/Library/LaunchAgents"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let plist = fs::read_to_string(plist).unwrap();
        assert!(plist.contains(&format!(
            "<key>HOME</key>\n\t\t<string>{}</string>",
            f.path("home").display()
        )));
    } else if LINUX {
        let unit = fs::read_to_string(f.unit(&f.vault_id())).unwrap();
        assert!(
            unit.contains(&format!(
                "Environment=\"HOME={}\"",
                f.path("home").display()
            )),
            "{unit}"
        );
    } else {
        assert!(!f.path("home/Library/LaunchAgents").exists());
        assert!(!f.path("home/calls").exists());
    }
    f.seed("other/.claude.json", "{\"owner\":true}\n");
    fs::create_dir_all(f.path("other-cache")).unwrap();
    let invoke = |args: &[&str]| {
        support::output(
            Command::new(f.path("bin/wi"))
                .args(args)
                .env("HOME", f.path("other"))
                .env("USERPROFILE", f.path("other"))
                .env("APPDATA", f.path("other"))
                .env("LOCALAPPDATA", f.path("other"))
                .env("CODEX_HOME", f.path("other/.codex"))
                .env("WI_VAULT", f.path("vault"))
                .env("WI_CACHE_DIR", f.path("other-cache"))
                .env("WI_LAUNCHCTL", f.path("bin/launchctl"))
                .env("WI_SYSTEMCTL", f.path("bin/systemctl"))
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("WI_FAULT"),
        )
    };
    // Force a pending skill sync without editing any owner note.
    fs::remove_file(f.path("vault/.wiki-island/local/skills-digest")).ok();
    let maintained = invoke(&["maintain"]);
    assert!(maintained.status.success(), "{maintained:?}");
    assert!(!f.path("other/.claude/skills").exists());
    assert!(!f.path("other/.agents/skills").exists());
    assert!(f.path("home/.claude/skills").is_dir());
    let removed = invoke(&["uninstall"]);
    assert!(removed.status.success(), "{removed:?}");
    assert_eq!(
        fs::read_to_string(f.path("home/.claude.json")).unwrap(),
        "{\"theme\":1}\n"
    );
    assert_eq!(
        fs::read_to_string(f.path("home/.codex/config.toml")).unwrap(),
        "# owner setting\n"
    );
    assert_eq!(
        fs::read_to_string(f.path("other/.claude.json")).unwrap(),
        "{\"owner\":true}\n"
    );
    assert!(fs::read_dir(f.path("cache")).unwrap().next().is_none());
    assert!(
        fs::read_dir(f.path("other-cache"))
            .unwrap()
            .next()
            .is_some()
    );
    if LINUX {
        assert!(!f.unit(&f.vault_id()).exists());
        assert!(!f.path("other/.config").exists());
    }
}

#[test]
fn watcher_continues_when_previous_log_is_a_directory() {
    let f = Fixture::new();
    assert!(f.run(&["setup"]).status.success());
    let config = wi_note::VaultConfig::parse(
        &fs::read_to_string(f.path("vault/.wiki-island/config.toml")).unwrap(),
    )
    .unwrap();
    #[cfg(target_os = "macos")]
    let log = wi_export::service::plan(
        &f.path("home"),
        &f.path("bin/wi"),
        &f.path("vault"),
        config.vault_id().unwrap(),
    )
    .unwrap()
    .log_path;
    #[cfg(not(target_os = "macos"))]
    let log = f.path(&format!(
        "home/Library/Logs/wiki-island/{}.log",
        config.vault_id().unwrap()
    ));
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(&log, vec![b'x'; 1_100_000]).unwrap();
    fs::create_dir(log.with_extension("log.old")).unwrap();
    let watched = f.run(&["watch", "--duration", "1"]);
    assert!(watched.status.success(), "{watched:?}");
    let errors = String::from_utf8(watched.stderr).unwrap();
    assert_eq!(
        errors.matches("log could not be trimmed").count(),
        usize::from(cfg!(target_os = "macos")),
        "{errors}"
    );
    if !cfg!(target_os = "macos") {
        assert_eq!(fs::metadata(log).unwrap().len(), 1_100_000);
    }
}
