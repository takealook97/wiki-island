//! Linux background care through a fake `systemctl --user`, with and without a user manager.
#![cfg(target_os = "linux")]
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
                    "show-environment) echo SECRET=1; exit {probe};;\n",
                    "is-enabled) test -f \"$HOME/enabled\"; exit $?;;\n",
                    "is-active) test -f \"$HOME/active\"; exit $?;;\n",
                    "enable) touch \"$HOME/enabled\";;\n",
                    "disable) rm -f \"$HOME/enabled\" \"$HOME/active\";;\n",
                    "start|restart) touch \"$HOME/active\";;\n",
                    "stop) rm -f \"$HOME/active\";;\n",
                    "esac\nexit 0\n",
                ),
                probe = if manager { 0 } else { 1 }
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
    fn run(&self, args: &[&str]) -> Output {
        Command::new(self.path("bin/wi"))
            .args(args)
            .env("HOME", self.path("home"))
            .env("CODEX_HOME", self.path("home/.codex"))
            .env("WI_VAULT", self.path("vault"))
            .env("WI_CACHE_DIR", self.path("cache"))
            .env("WI_SYSTEMCTL", self.path("bin/systemctl"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("WI_FAULT")
            .output()
            .unwrap()
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
