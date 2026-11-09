use super::*;
use std::cell::{Cell, RefCell};
use std::os::unix::process::ExitStatusExt;

const ID: &str = "vault_1-a";
const UNIT: &str = "wiki-island-vault_1-a.service";

fn output(code: i32, stderr: &str) -> Output {
    Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

/// An in-memory user manager. `fail` lists verbs that fail once each, in order of listing.
#[derive(Default)]
struct Fake {
    calls: RefCell<Vec<String>>,
    unavailable: bool,
    enabled: Cell<bool>,
    active: Cell<bool>,
    fail: RefCell<Vec<&'static str>>,
}

impl Fake {
    fn running() -> Self {
        let fake = Self::default();
        fake.enabled.set(true);
        fake.active.set(true);
        fake
    }
    fn failing(verbs: &[&'static str]) -> Self {
        let fake = Self::default();
        fake.fail.replace(verbs.to_vec());
        fake
    }
    fn calls(&self) -> Vec<String> {
        self.calls.take()
    }
}

impl Systemctl for Fake {
    fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        self.calls.borrow_mut().push(args.join(" "));
        let verb = args[0];
        let mut fail = self.fail.borrow_mut();
        if let Some(index) = fail.iter().position(|v| *v == verb) {
            fail.remove(index);
            return Ok(output(1, "Job failed."));
        }
        let ok = match verb {
            "show-environment" => !self.unavailable,
            "is-enabled" => self.enabled.get(),
            "is-active" => self.active.get(),
            "enable" => {
                self.enabled.set(true);
                true
            }
            "disable" => {
                self.enabled.set(false);
                if args.contains(&"--now") {
                    self.active.set(false);
                }
                true
            }
            "start" | "restart" => {
                self.active.set(true);
                true
            }
            "stop" => {
                self.active.set(false);
                true
            }
            "daemon-reload" => true,
            other => panic!("unexpected systemctl verb {other}"),
        };
        Ok(output(if ok { 0 } else { 3 }, ""))
    }
}

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    binary: PathBuf,
    vault: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    Env {
        config: home.join(".config"),
        binary: home.join("bin dir/wi"),
        vault: home.join("Notes & Things/vault"),
        home,
        _dir: dir,
    }
}

fn do_plan(e: &Env) -> ServicePlan {
    plan_at(&e.config, &e.home, &e.binary, &e.vault, ID, false).unwrap()
}

fn unit_file(e: &Env) -> PathBuf {
    e.config.join("systemd/user").join(UNIT)
}

#[test]
fn renders_literal_paths_for_systemd() {
    let text = render(
        "/opt/my bin/w\"i\\$HOME%h",
        "/home/u/My \"Notes\" \\ $HOME %h",
        "/home/u $x%n",
    );
    assert_eq!(
        text,
        concat!(
            "[Unit]\n",
            "Description=wiki-island watcher\n",
            "\n",
            "[Service]\n",
            "ExecStart=\"/opt/my bin/w\\\"i\\\\$$HOME%%h\" \"watch\" \"--vault\" ",
            "\"/home/u/My \\\"Notes\\\" \\\\ $$HOME %%h\"\n",
            "Restart=always\n",
            "RestartSec=5\n",
            "Environment=\"HOME=/home/u $x%%n\"\n",
            "\n",
            "[Install]\n",
            "WantedBy=default.target\n",
        )
    );
}

#[test]
fn plan_places_the_unit_under_the_config_home_and_carries_home() {
    let e = env();
    let p = do_plan(&e);
    assert_eq!(p.label, UNIT);
    assert_eq!(p.unit_path, unit_file(&e));
    assert!(p.existing.is_none());
    assert!(
        p.contents
            .contains(&format!("Environment=\"HOME={}\"", e.home.display()))
    );
    assert!(!e.config.exists(), "planning must not write");
    let elsewhere = e.home.join("xdg");
    let p = plan_at(&elsewhere, &e.home, &e.binary, &e.vault, ID, false).unwrap();
    assert_eq!(p.unit_path, elsewhere.join("systemd/user").join(UNIT));
    assert!(p.contents.contains(&format!("HOME={}", e.home.display())));
}

#[test]
fn plan_rejects_invalid_ids_relative_paths_and_dev_binaries() {
    let e = env();
    for bad in ["", "a/b", "a b", "../x", "a.b"] {
        assert!(matches!(
            plan_at(&e.config, &e.home, &e.binary, &e.vault, bad, false),
            Err(Error::Config(_))
        ));
    }
    assert!(plan_at(&e.config, &e.home, Path::new("wi"), &e.vault, ID, false).is_err());
    assert!(plan_at(&e.config, Path::new("home"), &e.binary, &e.vault, ID, false).is_err());
    let target = e.home.join("target/debug");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(e.home.join("target/CACHEDIR.TAG"), "cargo").unwrap();
    let dev = target.join("wi");
    assert!(plan_at(&e.config, &e.home, &dev, &e.vault, ID, false).is_err());
    assert!(plan_at(&e.config, &e.home, &dev, &e.vault, ID, true).is_ok());
}

#[test]
fn first_install_writes_enables_and_restarts() {
    let e = env();
    let fake = Fake::default();
    do_plan(&e).apply(&fake).unwrap();
    assert_eq!(
        fake.calls(),
        [
            format!("is-enabled {UNIT}"),
            format!("is-active {UNIT}"),
            "daemon-reload".into(),
            format!("enable {UNIT}"),
            format!("restart {UNIT}"),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(unit_file(&e)).unwrap(),
        do_plan(&e).contents
    );
    assert!(fake.enabled.get() && fake.active.get());
}

#[test]
fn identical_enabled_and_active_unit_is_left_alone() {
    let e = env();
    let fake = Fake::default();
    do_plan(&e).apply(&fake).unwrap();
    fake.calls();
    let again = do_plan(&e);
    assert!(again.is_noop());
    assert!(again.diff().is_empty());
    again.apply(&fake).unwrap();
    assert_eq!(
        fake.calls(),
        [format!("is-enabled {UNIT}"), format!("is-active {UNIT}")]
    );
}

#[test]
fn identical_but_stopped_unit_is_started_not_restarted() {
    let e = env();
    let fake = Fake::default();
    do_plan(&e).apply(&fake).unwrap();
    fake.active.set(false);
    fake.calls();
    do_plan(&e).apply(&fake).unwrap();
    assert_eq!(fake.calls().last().unwrap(), &format!("start {UNIT}"));
    assert!(fake.active.get());
}

#[test]
fn changed_unit_while_active_is_restarted_and_backed_up() {
    let e = env();
    let fake = Fake::default();
    do_plan(&e).apply(&fake).unwrap();
    let moved = e.home.join("other/wi");
    let p = plan_at(&e.config, &e.home, &moved, &e.vault, ID, false).unwrap();
    assert!(
        p.diff()
            .contains(&format!("+ExecStart=\"{}\"", moved.display()))
    );
    fake.calls();
    p.apply(&fake).unwrap();
    assert_eq!(
        fake.calls(),
        [
            format!("is-enabled {UNIT}"),
            format!("is-active {UNIT}"),
            "daemon-reload".into(),
            format!("enable {UNIT}"),
            format!("restart {UNIT}"),
        ]
    );
    assert_eq!(std::fs::read_to_string(unit_file(&e)).unwrap(), p.contents);
    let backups = std::fs::read_dir(e.vault.join(".wiki-island/local/backups"))
        .unwrap()
        .filter(|d| {
            d.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("wi-backup-")
        })
        .count();
    assert_eq!(backups, 1);
}

#[test]
fn restart_failure_after_enable_rolls_back_a_first_install() {
    let e = env();
    let fake = Fake::failing(&["restart"]);
    let err = do_plan(&e).apply(&fake).unwrap_err();
    assert!(
        err.to_string().contains("systemctl --user restart"),
        "{err}"
    );
    assert_eq!(
        fake.calls(),
        [
            format!("is-enabled {UNIT}"),
            format!("is-active {UNIT}"),
            "daemon-reload".into(),
            format!("enable {UNIT}"),
            format!("restart {UNIT}"),
            format!("stop {UNIT}"),
            format!("disable {UNIT}"),
            "daemon-reload".into(),
        ]
    );
    assert!(!unit_file(&e).exists());
    assert!(!fake.enabled.get() && !fake.active.get());
}

#[test]
fn replacement_failure_restores_previous_file_enablement_and_process() {
    let e = env();
    let fake = Fake::default();
    let first = do_plan(&e);
    first.apply(&fake).unwrap();
    let moved = e.home.join("other/wi");
    let p = plan_at(&e.config, &e.home, &moved, &e.vault, ID, false).unwrap();
    fake.fail.replace(vec!["restart"]);
    fake.calls();
    assert!(p.apply(&fake).is_err());
    assert_eq!(
        fake.calls()[5..],
        [
            "daemon-reload".to_owned(),
            format!("enable {UNIT}"),
            format!("restart {UNIT}"),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(unit_file(&e)).unwrap(),
        first.contents
    );
    assert!(fake.enabled.get() && fake.active.get());
}

#[test]
fn replacement_failure_keeps_a_previously_disabled_unit_disabled_and_stopped() {
    let e = env();
    let fake = Fake::default();
    let first = do_plan(&e);
    first.apply(&fake).unwrap();
    fake.enabled.set(false);
    fake.active.set(false);
    let moved = e.home.join("other/wi");
    let p = plan_at(&e.config, &e.home, &moved, &e.vault, ID, false).unwrap();
    fake.fail.replace(vec!["restart"]);
    fake.calls();
    let err = p.apply(&fake).unwrap_err();
    assert!(err.to_string().contains("restart"), "{err}");
    assert_eq!(
        fake.calls()[5..],
        [
            "daemon-reload".to_owned(),
            format!("disable {UNIT}"),
            format!("stop {UNIT}"),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(unit_file(&e)).unwrap(),
        first.contents
    );
    assert!(!fake.enabled.get() && !fake.active.get());
}

#[test]
fn stale_preview_and_symlinked_unit_fail_before_systemctl() {
    let e = env();
    let fake = Fake::default();
    let p = do_plan(&e);
    std::fs::create_dir_all(unit_file(&e).parent().unwrap()).unwrap();
    std::fs::write(unit_file(&e), "owner edit\n").unwrap();
    assert!(matches!(p.apply(&fake), Err(Error::Stale)));
    assert!(fake.calls().is_empty());
    std::fs::remove_file(unit_file(&e)).unwrap();
    let target = e.home.join("foreign");
    std::fs::write(&target, "keep\n").unwrap();
    std::os::unix::fs::symlink(&target, unit_file(&e)).unwrap();
    assert!(matches!(
        plan_at(&e.config, &e.home, &e.binary, &e.vault, ID, false),
        Err(Error::Config(_))
    ));
    assert_eq!(std::fs::read_to_string(target).unwrap(), "keep\n");
}

#[test]
fn uninstall_disables_removes_and_reloads() {
    let e = env();
    let fake = Fake::default();
    do_plan(&e).apply(&fake).unwrap();
    fake.calls();
    assert!(uninstall_at(&e.config, ID, &fake).unwrap());
    assert_eq!(
        fake.calls(),
        [
            "show-environment".to_owned(),
            format!("disable --now {UNIT}"),
            "daemon-reload".into(),
        ]
    );
    assert!(!unit_file(&e).exists());
    assert!(!fake.enabled.get() && !fake.active.get());
}

#[test]
fn uninstall_tolerates_a_unit_that_is_not_loaded() {
    let e = env();
    struct NotLoaded(RefCell<Vec<String>>);
    impl Systemctl for NotLoaded {
        fn run(&self, args: &[&str]) -> std::io::Result<Output> {
            self.0.borrow_mut().push(args.join(" "));
            Ok(match args[0] {
                "stop" => output(5, "Failed to stop x.service: Unit x.service not loaded."),
                _ => output(0, ""),
            })
        }
    }
    let fake = NotLoaded(RefCell::default());
    assert!(!uninstall_at(&e.config, ID, &fake).unwrap());
    assert_eq!(
        fake.0.take(),
        [
            "show-environment".to_owned(),
            format!("stop {UNIT}"),
            "daemon-reload".into()
        ]
    );
}

#[test]
fn uninstall_reports_other_failures() {
    let e = env();
    let fake = Fake::default();
    do_plan(&e).apply(&fake).unwrap();
    fake.fail.replace(vec!["disable"]);
    assert!(matches!(
        uninstall_at(&e.config, ID, &fake),
        Err(Error::Service(_))
    ));
    assert!(unit_file(&e).exists());
}

#[test]
fn status_separates_configured_from_running() {
    let e = env();
    let fake = Fake::default();
    let s = status_at(&e.config, ID, &fake).unwrap();
    assert!(!s.installed && !s.configured() && !s.running);
    do_plan(&e).apply(&fake).unwrap();
    let s = status_at(&e.config, ID, &fake).unwrap();
    assert!(s.configured() && s.running);
    fake.active.set(false);
    let s = status_at(&e.config, ID, &fake).unwrap();
    assert!(s.configured() && !s.running);
    fake.enabled.set(false);
    assert!(!status_at(&e.config, ID, &fake).unwrap().configured());
    let json = serde_json::to_value(&s).unwrap();
    assert_eq!(json["label"], UNIT);
    assert!(json["unit_path"].is_string());
}

#[test]
fn unavailable_manager_is_detected_and_uninstall_still_cleans_up() {
    let e = env();
    let fake = Fake::running();
    do_plan(&e).apply(&fake).unwrap();
    let wants = e.config.join("systemd/user").join(WANTS_DIR);
    std::fs::create_dir_all(&wants).unwrap();
    std::os::unix::fs::symlink(unit_file(&e), wants.join(UNIT)).unwrap();
    let gone = Fake {
        unavailable: true,
        ..Fake::default()
    };
    assert!(!available(&gone));
    let err = status_at(&e.config, ID, &gone).unwrap_err();
    assert!(err.to_string().contains("run wi watch"), "{err}");
    assert!(uninstall_at(&e.config, ID, &gone).unwrap());
    assert_eq!(gone.calls(), ["show-environment"; 3]);
    assert!(!unit_file(&e).exists());
    assert!(std::fs::symlink_metadata(wants.join(UNIT)).is_err());
}
