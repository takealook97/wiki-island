#![cfg(windows)]

use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    binary: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("profile");
        let binary = temp.path().join("bin/wi.exe");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_wi"), &binary).unwrap();
        Self {
            _temp: temp,
            home,
            binary,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .env("HOME", self._temp.path().join("unused-home"))
            .env("USERPROFILE", &self.home)
            .env("APPDATA", self._temp.path().join("roaming"))
            .env("LOCALAPPDATA", self._temp.path().join("local"))
            .env("WI_DATA_DIR", self._temp.path().join("data"))
            .env("WI_CACHE_DIR", self._temp.path().join("cache"))
            .env_remove("WI_VAULT")
            .env_remove("WI_FAULT")
            .env_remove("CODEX_HOME")
            .current_dir(self._temp.path());
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}

#[test]
fn setup_uses_profile_and_roaming_paths_skips_service_and_uninstalls() {
    let fixture = Fixture::new();
    for directory in ["Claude", "Code/User", "Zed"] {
        std::fs::create_dir_all(fixture._temp.path().join("roaming").join(directory)).unwrap();
    }
    let output = fixture.run(&["setup", "--json"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        fixture
            .home
            .join("wiki-island/.wiki-island/config.toml")
            .is_file()
    );
    for config in [
        "Claude/claude_desktop_config.json",
        "Code/User/mcp.json",
        "Zed/settings.json",
    ] {
        let path = fixture._temp.path().join("roaming").join(config);
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("wiki-island")
        );
    }
    assert!(!fixture.home.join("Library/LaunchAgents").exists());
    let service = fixture.run(&["service", "install"]);
    assert!(!service.status.success());
    assert!(
        String::from_utf8_lossy(&service.stderr).contains("run wi watch"),
        "{service:?}"
    );
    let removed = fixture.run(&["uninstall", "--json"]);
    assert!(removed.status.success(), "{removed:?}");
    assert!(
        fixture
            .home
            .join("wiki-island/.wiki-island/config.toml")
            .is_file()
    );
}

#[test]
fn setup_falls_back_to_home_drive_and_home_path() {
    let fixture = Fixture::new();
    let home = fixture.home.to_str().unwrap();
    let output = fixture
        .command()
        .env_remove("USERPROFILE")
        .env("HOMEDRIVE", &home[..2])
        .env("HOMEPATH", &home[2..])
        .args(["setup", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        fixture
            .home
            .join("wiki-island/.wiki-island/config.toml")
            .is_file()
    );
}
