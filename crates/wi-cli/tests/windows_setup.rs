#![cfg(windows)]

use std::path::PathBuf;
use std::process::{Command, Output};

/// A stand-in for `schtasks.exe` that keeps one task in a state folder: `task.xml` holds the
/// registered definition, `running` marks a started task, `unavailable` makes every call fail.
const FAKE_SCHTASKS: &str = r#"@echo off
setlocal
set "state=%WI_FAKE_SCHTASKS_DIR%"
if exist "%state%\unavailable" exit /b 1
echo %*>>"%state%\calls.txt"
if /I "%~1"=="/Query" goto query
if /I "%~1"=="/Create" goto create
if /I "%~1"=="/Run" goto run
if /I "%~1"=="/End" goto end
if /I "%~1"=="/Delete" goto delete
exit /b 1
:query
if /I "%~2"=="/FO" exit /b 0
if not exist "%state%\task.xml" exit /b 1
if /I "%~4"=="/XML" goto export
if exist "%state%\running" (echo "PC","\%~3","N/A","Running","Interactive only") else (echo "PC","\%~3","N/A","Ready","Interactive only")
exit /b 0
:export
type "%state%\task.xml"
exit /b 0
:create
copy /y "%~3" "%state%\task.xml" >nul
exit /b 0
:run
if not exist "%state%\task.xml" exit /b 1
type nul>"%state%\running"
exit /b 0
:end
if not exist "%state%\running" exit /b 1
del "%state%\running"
exit /b 0
:delete
if not exist "%state%\task.xml" exit /b 1
del "%state%\task.xml"
if exist "%state%\running" del "%state%\running"
exit /b 0
"#;

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    binary: PathBuf,
    schtasks: PathBuf,
    state: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("profile");
        let binary = temp.path().join("bin/wi.exe");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_wi"), &binary).unwrap();
        let schtasks = temp.path().join("fake-schtasks.cmd");
        std::fs::write(&schtasks, FAKE_SCHTASKS.replace('\n', "\r\n")).unwrap();
        let state = temp.path().join("scheduler");
        std::fs::create_dir_all(&state).unwrap();
        Self {
            _temp: temp,
            home,
            binary,
            schtasks,
            state,
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
            .env("WI_SCHTASKS", &self.schtasks)
            .env("WI_FAKE_SCHTASKS_DIR", &self.state)
            .env_remove("WI_VAULT")
            .env_remove("WI_FAULT")
            .env_remove("CODEX_HOME")
            .current_dir(self._temp.path());
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn task(&self) -> Option<Vec<u8>> {
        std::fs::read(self.state.join("task.xml")).ok()
    }
    fn running(&self) -> bool {
        self.state.join("running").exists()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The registered definition, decoded from the UTF-16 file setup hands to schtasks.
fn definition(bytes: &[u8]) -> String {
    assert_eq!(&bytes[..2], &[0xFF, 0xFE], "definition must be UTF-16");
    let units: Vec<u16> = bytes[2..]
        .chunks(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).unwrap()
}

#[test]
fn setup_uses_profile_and_roaming_paths_registers_the_task_and_uninstalls() {
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
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["background"], true, "{json}");
    let task = definition(&fixture.task().expect("setup registers the task"));
    assert!(task.contains("Managed by wiki-island. Vault "), "{task}");
    assert!(task.contains("<LogonType>InteractiveToken</LogonType>"));
    // The program path is the absolute invocation path, with native separators.
    let binary = std::path::absolute(&fixture.binary).unwrap();
    assert!(
        task.contains(&format!("<Command>{}</Command>", binary.display())),
        "{task}"
    );
    // Setup records the canonical vault folder, without the `\\?\` prefix.
    let vault = std::fs::canonicalize(fixture.home.join("wiki-island")).unwrap();
    let vault = vault.to_str().unwrap().trim_start_matches(r"\\?\");
    assert!(
        task.contains(&format!("<WorkingDirectory>{}</WorkingDirectory>", vault)),
        "{task}"
    );
    assert!(fixture.running());
    // Setup again: the matching running task is left alone.
    let calls = std::fs::read_to_string(fixture.state.join("calls.txt")).unwrap();
    assert!(fixture.run(&["setup"]).status.success());
    let again = std::fs::read_to_string(fixture.state.join("calls.txt")).unwrap();
    assert!(!again[calls.len()..].contains("/Create"), "{again}");
    assert!(!again[calls.len()..].contains("/Run"), "{again}");

    let preview = fixture.run(&["service", "install"]);
    assert!(preview.status.success(), "{preview:?}");
    assert!(
        text(&preview.stdout).contains("already matches"),
        "{preview:?}"
    );
    let status = fixture.run(&["service", "status", "--json"]);
    assert!(status.status.success(), "{status:?}");
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["installed"], true);
    assert_eq!(status["running"], true);
    assert!(
        status["task_name"]
            .as_str()
            .unwrap()
            .starts_with("wiki-island-")
    );
    let doctor = fixture.run(&["doctor"]);
    assert!(
        text(&doctor.stdout).contains("[ok] service: Background care is running."),
        "{doctor:?}"
    );

    let removed = fixture.run(&["uninstall", "--json"]);
    assert!(removed.status.success(), "{removed:?}");
    assert!(fixture.task().is_none());
    assert!(!fixture.running());
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

#[test]
fn empty_profile_variables_fall_back_instead_of_using_the_working_directory() {
    let fixture = Fixture::new();
    let home = fixture.home.to_str().unwrap();
    let output = fixture
        .command()
        .env("USERPROFILE", "")
        .env("HOMEDRIVE", &home[..2])
        .env("HOMEPATH", &home[2..])
        .args(["setup", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    // The fallback profile is used ...
    assert!(
        fixture
            .home
            .join("wiki-island/.wiki-island/config.toml")
            .is_file()
    );
    // ... and nothing is created relative to the working directory.
    assert!(!fixture._temp.path().join("wiki-island").exists());
}

#[test]
fn relative_profile_is_ignored_in_favor_of_an_absolute_fallback() {
    let fixture = Fixture::new();
    let home = fixture.home.to_str().unwrap();
    let output = fixture
        .command()
        .env("USERPROFILE", "elsewhere")
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
    assert!(!fixture._temp.path().join("elsewhere").exists());
}

#[test]
fn empty_appdata_never_targets_a_relative_app_config() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.home.join("AppData/Roaming/Claude")).unwrap();
    std::fs::create_dir_all(fixture._temp.path().join("Claude")).unwrap();
    let output = fixture
        .command()
        .env("APPDATA", "")
        .args(["setup", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    // An empty APPDATA must not resolve to <cwd>\Claude\claude_desktop_config.json.
    assert!(
        !fixture
            ._temp
            .path()
            .join("Claude/claude_desktop_config.json")
            .exists()
    );
}

#[test]
fn stopped_task_is_reported_by_doctor_and_restarted_by_setup() {
    let fixture = Fixture::new();
    assert!(fixture.run(&["setup"]).status.success());
    std::fs::remove_file(fixture.state.join("running")).unwrap();
    let doctor = fixture.run(&["doctor"]);
    let report = text(&doctor.stdout);
    assert!(
        report.contains("[warn] service: Background care is not running."),
        "{report}"
    );
    let status = fixture.run(&["service", "status"]);
    assert!(
        text(&status.stdout).contains("registered, not running"),
        "{status:?}"
    );
    assert!(fixture.run(&["setup"]).status.success());
    assert!(fixture.running());
}

#[test]
fn without_task_scheduler_setup_skips_the_service_and_doctor_says_what_to_do() {
    let fixture = Fixture::new();
    std::fs::write(fixture.state.join("unavailable"), "").unwrap();
    let output = fixture.run(&["setup", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["background"], false, "{json}");
    let human = fixture.run(&["setup"]);
    assert!(
        text(&human.stdout).contains("background care needs Windows Task Scheduler"),
        "{human:?}"
    );
    let doctor = fixture.run(&["doctor"]);
    assert!(
        text(&doctor.stdout).contains(
            "[warn] service: Background care needs Windows Task Scheduler, which is not available on this computer; run wi watch to keep your notes up to date."
        ),
        "{doctor:?}"
    );
    let install = fixture.run(&["service", "install", "--yes"]);
    assert!(!install.status.success());
    assert!(
        text(&install.stderr).contains("run wi watch"),
        "{install:?}"
    );
    let removed = fixture.run(&["uninstall"]);
    assert!(removed.status.success(), "{removed:?}");
    assert!(fixture.task().is_none());
}
