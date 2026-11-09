//! Linux background care through a fake `systemctl --user`, with and without a user manager.
#![cfg(target_os = "linux")]
mod support;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};
use tempfile::TempDir;

struct Fixture(TempDir);

impl Fixture {
    /// `manager` false makes `show-environment` fail like a host without a user session.
    fn new(manager: bool) -> Self {
        Self::with_probe(if manager { "exit 0" } else { "exit 1" })
    }
    /// A manager that answers the first probe and then disappears, like a session that ends
    /// while setup runs.
    fn flapping() -> Self {
        let f = Self::with_probe(
            "test ! -f \"$HOME/probed\" || exit 1; touch \"$HOME/probed\"; exit 0",
        );
        let fake = f.path("bin/systemctl");
        let script = fs::read_to_string(&fake).unwrap().replacen(
            "case $1 in\n",
            "test ! -f \"$HOME/probed\" || test \"$1\" = show-environment || { echo 'Failed to connect to bus: No medium found' >&2; exit 1; }\ncase $1 in\n",
            1,
        );
        fs::write(&fake, script).unwrap();
        f
    }
    fn with_probe(probe: &str) -> Self {
        let f = Self(tempfile::tempdir().unwrap());
        fs::create_dir_all(f.path("home")).unwrap();
        fs::create_dir_all(f.path("bin")).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_wi"), f.path("bin/wi")).unwrap();
        let fake = f.path("bin/systemctl");
        fs::write(
            &fake,
            format!(
                concat!(
                    "#!/bin/sh\nshift\necho \"$*\" >> \"$HOME/calls\"\ncase $1 in\n",
                    "show-environment) echo SECRET=1; {probe};;\n",
                    "is-enabled) if test -f \"$HOME/enabled\"; then echo enabled; else echo disabled; exit 1; fi;;\n",
                    "is-active) if test -f \"$HOME/active\"; then echo active; else echo inactive; exit 3; fi;;\n",
                    "enable) touch \"$HOME/enabled\";;\n",
                    "disable) rm -f \"$HOME/enabled\" \"$HOME/active\";;\n",
                    "start|restart) touch \"$HOME/active\";;\n",
                    "stop) rm -f \"$HOME/active\";;\n",
                    "esac\nexit 0\n",
                ),
                probe = probe
            ),
        )
        .unwrap();
        fs::set_permissions(fake, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(f.path("home/.claude.json"), "{}\n").unwrap();
        f
    }
    fn path(&self, path: &str) -> PathBuf {
        self.0.path().join(path)
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(self.path("bin/wi"));
        command
            .args(args)
            .env("HOME", self.path("home"))
            .env("CODEX_HOME", self.path("home/.codex"))
            .env("WI_VAULT", self.path("vault"))
            .env("WI_CACHE_DIR", self.path("cache"))
            .env("WI_SYSTEMCTL", self.path("bin/systemctl"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("WI_FAULT");
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        support::output(&mut self.command(args))
    }
    fn calls(&self) -> String {
        fs::read_to_string(self.path("home/calls")).unwrap_or_default()
    }
    fn units(&self) -> Vec<PathBuf> {
        fs::read_dir(self.path("home/.config/systemd/user"))
            .into_iter()
            .flatten()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "service"))
            .collect()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn setup_doctor_and_uninstall_manage_a_user_unit() {
    let f = Fixture::new(true);
    let out = f.run(&["setup", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["background"], true, "{json}");
    let units = f.units();
    assert_eq!(units.len(), 1);
    let unit = fs::read_to_string(&units[0]).unwrap();
    assert!(unit.contains(&format!(
        "ExecStart=\"{}\" \"watch\" \"--vault\" \"{}\"",
        f.path("bin/wi").display(),
        f.path("vault").canonicalize().unwrap().display()
    )));
    assert!(unit.contains("Restart=always"));
    let calls = f.calls();
    assert!(calls.contains("daemon-reload"), "{calls}");
    assert!(calls.contains("enable wiki-island-"), "{calls}");
    assert!(f.path("home/active").exists());

    let doctor = f.run(&["doctor"]);
    let report = text(&doctor.stdout);
    assert!(
        report.contains("[ok] service: Background care is running."),
        "{report}"
    );
    assert!(!report.contains("SECRET"), "{report}");
    let status = f.run(&["service", "status", "--json"]);
    assert!(status.status.success(), "{status:?}");
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["running"], true);

    let out = f.run(&["uninstall"]);
    assert!(out.status.success(), "{out:?}");
    assert!(f.units().is_empty());
    assert!(!f.path("home/.config/systemd").exists());
    assert!(f.calls().contains("disable --now wiki-island-"));
    assert!(!f.path("home/enabled").exists() && !f.path("home/active").exists());
}

#[test]
fn explicit_service_commands_install_and_remove_the_unit() {
    let f = Fixture::new(true);
    assert!(f.run(&["init"]).status.success());
    let preview = f.run(&["service", "install", "--allow-dev-binary"]);
    assert!(preview.status.success(), "{preview:?}");
    assert!(text(&preview.stdout).contains("Preview only"));
    assert!(f.units().is_empty());
    let out = f.run(&["service", "install", "--yes", "--allow-dev-binary"]);
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out.stdout).contains("and started it"));
    assert_eq!(f.units().len(), 1);
    let out = f.run(&["service", "uninstall"]);
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out.stdout).contains("unit removed"));
    assert!(f.units().is_empty());
}

#[test]
fn without_a_user_session_setup_skips_the_service_and_doctor_says_what_to_do() {
    let f = Fixture::new(false);
    let out = f.run(&["setup", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["background"], false, "{json}");
    assert!(f.units().is_empty());
    assert!(!f.path("home/.config").exists());
    assert_eq!(f.calls().trim(), "show-environment");
    let human = f.run(&["setup"]);
    assert!(human.status.success(), "{human:?}");
    assert!(
        text(&human.stdout).contains("background care needs a systemd user session"),
        "{human:?}"
    );

    let doctor = f.run(&["doctor"]);
    assert!(doctor.status.success(), "{doctor:?}");
    let report = text(&doctor.stdout);
    let line = report
        .lines()
        .find(|l| l.contains("service:"))
        .expect("doctor reports the service");
    assert_eq!(
        line,
        "[warn] service: Background care needs a systemd user session; until you log in to one, run wi watch to keep your notes up to date."
    );
    assert!(!report.contains("Next: run wi setup"), "{report}");
    assert!(!report.contains("SECRET"), "{report}");

    let install = f.run(&["service", "install", "--yes"]);
    assert!(!install.status.success());
    let message = text(&install.stderr);
    assert!(message.contains("systemd user session"), "{message}");
    assert!(message.contains("run wi watch"), "{message}");
    assert!(!message.contains("SECRET"), "{message}");

    let out = f.run(&["uninstall"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        f.calls().lines().all(|call| call == "show-environment"),
        "{}",
        f.calls()
    );
}

#[test]
fn a_manager_lost_after_preflight_still_finishes_setup_with_the_skip() {
    let f = Fixture::flapping();
    let out = f.run(&["setup", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["background"], false, "{json}");
    assert!(f.units().is_empty());
    let calls = f.calls();
    // Only probes ran: nothing was enabled, started or reloaded.
    assert!(
        calls
            .lines()
            .all(|call| call == "show-environment" || call.starts_with("is-")),
        "{calls}"
    );
    // A second run without a manager also finishes and says what to do.
    let human = f.run(&["setup"]);
    assert!(human.status.success(), "{human:?}");
    assert!(
        text(&human.stdout).contains("background care needs a systemd user session"),
        "{human:?}"
    );
}

#[test]
fn uninstall_after_a_config_home_change_removes_the_installed_unit() {
    let f = Fixture::new(true);
    assert!(f.run(&["setup"]).status.success());
    assert_eq!(f.units().len(), 1);
    let out = support::output(
        f.command(&["uninstall"])
            .env("XDG_CONFIG_HOME", f.path("home/other-config")),
    );
    assert!(out.status.success(), "{out:?}");
    assert!(f.units().is_empty(), "{:?}", f.units());
    assert!(!f.path("home/enabled").exists());
}

#[test]
fn setup_refuses_a_foreign_unit_with_the_same_name() {
    let f = Fixture::new(true);
    assert!(f.run(&["init"]).status.success());
    let id = wi_note::VaultConfig::parse(
        &fs::read_to_string(f.path("vault/.wiki-island/config.toml")).unwrap(),
    )
    .unwrap()
    .vault_id()
    .unwrap()
    .to_owned();
    let unit = f.path(&format!(
        "home/.config/systemd/user/wiki-island-{id}.service"
    ));
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    let foreign = "[Service]\nExecStart=/bin/sleep infinity\n";
    fs::write(&unit, foreign).unwrap();
    assert!(!f.run(&["setup"]).status.success());
    let out = f.run(&["service", "uninstall"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("different service unit"),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(&unit).unwrap(), foreign);
    assert!(!f.calls().contains("disable"), "{}", f.calls());
}
