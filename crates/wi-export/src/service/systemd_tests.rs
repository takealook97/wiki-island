use super::*;
use std::cell::{Cell, RefCell};
use std::os::unix::process::ExitStatusExt;

const ID: &str = "vault_1-a";
const UNIT: &str = "wiki-island-vault_1-a.service";

fn output(code: i32, stderr: &str) -> Output {
    answer(code, "", stderr)
}

fn answer(code: i32, stdout: &str, stderr: &str) -> Output {
    Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
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
            // Real systemctl answers both probes on stdout.
            "is-enabled" if self.enabled.get() => return Ok(answer(0, "enabled\n", "")),
            "is-enabled" => return Ok(answer(1, "disabled\n", "")),
            "is-active" if self.active.get() => return Ok(answer(0, "active\n", "")),
            "is-active" => return Ok(answer(3, "inactive\n", "")),
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
        ID,
        "/opt/my bin/w$i%h",
        "/home/u/My \"Notes\" \\ $HOME %h",
        "/home/u $x%n",
    );
    assert_eq!(
        text,
        concat!(
            "# Managed by wiki-island. Vault vault_1-a.\n",
            "[Unit]\n",
            "Description=wiki-island watcher\n",
            "\n",
            "[Service]\n",
            "ExecStart=\"/opt/my bin/w$i%%h\" \"watch\" \"--vault\" ",
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
    let fake = Fake::default();
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

// Regression tests for the Linux review findings.

fn install_with_wants_link(e: &Env, fake: &Fake) -> PathBuf {
    do_plan(e).apply(fake).unwrap();
    let wants = unit_file(e).parent().unwrap().join(WANTS_DIR).join(UNIT);
    std::fs::create_dir_all(wants.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(unit_file(e), &wants).unwrap();
    wants
}

fn write_foreign(e: &Env) -> &'static str {
    let foreign = "[Service]\nExecStart=/bin/sleep infinity\n[Install]\nWantedBy=default.target\n";
    std::fs::create_dir_all(unit_file(e).parent().unwrap()).unwrap();
    std::fs::write(unit_file(e), foreign).unwrap();
    foreign
}

#[test]
fn executable_path_keeps_a_dollar_literal() {
    let e = env();
    let binary = e.home.join("bin$/한 글/wi");
    let p = plan_at(&e.config, &e.home, &binary, &e.vault, ID, false).unwrap();
    assert!(
        p.contents
            .contains(&format!("ExecStart=\"{}\" \"watch\"", binary.display())),
        "{}",
        p.contents
    );
}

#[test]
fn plan_rejects_a_binary_path_systemd_cannot_run() {
    let e = env();
    for name in ["w\"i", "w\\i"] {
        let binary = e.home.join(name);
        let err = plan_at(&e.config, &e.home, &binary, &e.vault, ID, false).unwrap_err();
        assert!(err.to_string().contains("quote or backslash"), "{err}");
    }
}

#[test]
fn rendered_unit_starts_with_the_ownership_marker() {
    let e = env();
    assert_eq!(
        do_plan(&e).contents.lines().next(),
        Some("# Managed by wiki-island. Vault vault_1-a.")
    );
}

#[test]
fn foreign_unit_with_the_same_name_is_never_replaced() {
    let e = env();
    let foreign = write_foreign(&e);
    let err = plan_at(&e.config, &e.home, &e.binary, &e.vault, ID, false).unwrap_err();
    assert!(err.to_string().contains("different service unit"), "{err}");
    // A foreign file that appears after planning is refused before any manager call.
    std::fs::remove_file(unit_file(&e)).unwrap();
    let p = do_plan(&e);
    std::fs::write(unit_file(&e), foreign).unwrap();
    let mut stale = p;
    stale.existing = Some(foreign.into());
    let fake = Fake::running();
    let err = stale.apply(&fake).unwrap_err();
    assert!(err.to_string().contains("different service unit"), "{err}");
    assert!(fake.calls().is_empty());
    assert_eq!(std::fs::read_to_string(unit_file(&e)).unwrap(), foreign);
}

#[test]
fn foreign_unit_with_the_same_name_is_never_uninstalled() {
    let e = env();
    let foreign = write_foreign(&e);
    for fake in [
        Fake::running(),
        Fake {
            unavailable: true,
            ..Fake::default()
        },
    ] {
        let err = uninstall_at(&e.config, ID, &fake).unwrap_err();
        assert!(err.to_string().contains("different service unit"), "{err}");
        assert!(fake.calls().is_empty());
        assert_eq!(std::fs::read_to_string(unit_file(&e)).unwrap(), foreign);
    }
}

#[test]
fn failed_snapshot_aborts_before_any_change() {
    let e = env();
    let fake = Fake::default();
    let first = do_plan(&e);
    first.apply(&fake).unwrap();
    let moved = e.home.join("new/wi");
    let p = plan_at(&e.config, &e.home, &moved, &e.vault, ID, false).unwrap();
    for verb in ["is-enabled", "is-active"] {
        fake.fail.replace(vec![verb, "restart"]);
        fake.calls();
        let err = p.apply(&fake).unwrap_err();
        assert!(err.to_string().contains("does not understand"), "{err}");
        let calls = fake.calls();
        assert!(calls.iter().all(|c| c.starts_with("is-")), "{calls:?}");
        assert!(fake.enabled.get() && fake.active.get());
        assert_eq!(
            std::fs::read_to_string(unit_file(&e)).unwrap(),
            first.contents
        );
    }
}

#[test]
fn snapshot_reads_stdout_not_exit_codes() {
    let ok = |stdout: &str, code| answer(code, stdout, "");
    assert_eq!(parse_enabled(&ok("enabled\n", 0)), Some(true));
    assert_eq!(parse_enabled(&ok("static\n", 0)), Some(false));
    assert_eq!(parse_enabled(&ok("not-found\n", 4)), Some(false));
    assert_eq!(
        parse_enabled(&answer(
            1,
            "",
            "Failed to get unit file state for x.service: No such file or directory"
        )),
        Some(false)
    );
    assert_eq!(parse_enabled(&ok("", 1)), None);
    assert_eq!(parse_enabled(&ok("enabled\n", 1)), Some(true));
    assert_eq!(parse_active(&ok("active\n", 0)), Some(true));
    assert_eq!(parse_active(&ok("failed\n", 3)), Some(false));
    assert_eq!(parse_active(&ok("", 0)), None);
    assert_eq!(
        parse_active(&answer(1, "", "Failed to connect to bus: No medium found")),
        None
    );
}

#[test]
fn absent_file_with_an_active_unit_is_refused_before_any_change() {
    let e = env();
    let fake = Fake::default();
    fake.active.set(true);
    let err = do_plan(&e).apply(&fake).unwrap_err();
    assert!(err.to_string().contains("different service unit"), "{err}");
    let calls = fake.calls();
    assert!(calls.iter().all(|c| c.starts_with("is-")), "{calls:?}");
    assert!(fake.active.get());
    assert!(!unit_file(&e).exists());
    assert!(!record_path(&e.vault).exists());
}

#[test]
fn uninstall_with_a_missing_file_removes_the_owned_wants_link() {
    let e = env();
    let fake = Fake::default();
    let wants = install_with_wants_link(&e, &fake);
    std::fs::remove_file(unit_file(&e)).unwrap();
    assert!(!uninstall_at(&e.config, ID, &fake).unwrap());
    assert!(std::fs::symlink_metadata(&wants).is_err());
}

#[test]
fn uninstall_leaves_a_wants_link_to_another_file() {
    let e = env();
    let fake = Fake::running();
    let wants = e.config.join("systemd/user").join(WANTS_DIR).join(UNIT);
    std::fs::create_dir_all(wants.parent().unwrap()).unwrap();
    let other = e.home.join("elsewhere.service");
    std::os::unix::fs::symlink(&other, &wants).unwrap();
    let gone = Fake {
        unavailable: true,
        ..Fake::default()
    };
    assert!(!uninstall_at(&e.config, ID, &gone).unwrap());
    assert!(!uninstall_at(&e.config, ID, &fake).unwrap());
    assert_eq!(std::fs::read_link(&wants).unwrap(), other);
}

#[test]
fn recorded_unit_path_survives_a_config_home_change() {
    let e = env();
    let fake = Fake::default();
    let wants = install_with_wants_link(&e, &fake);
    let other = e.home.join("other-config");
    assert!(uninstall_recorded_at(&other, &e.vault, ID, &fake).unwrap());
    assert!(!unit_file(&e).exists());
    assert!(std::fs::symlink_metadata(&wants).is_err());
    assert!(!fake.enabled.get() && !fake.active.get());
    assert!(!record_path(&e.vault).exists());
}

#[test]
fn identical_unit_still_records_its_path() {
    let e = env();
    let fake = Fake::default();
    do_plan(&e).apply(&fake).unwrap();
    std::fs::remove_file(record_path(&e.vault)).unwrap();
    do_plan(&e).apply(&fake).unwrap();
    assert_eq!(recorded_units(&e.vault, UNIT).unwrap(), [unit_file(&e)]);
}

fn other_config(e: &Env) -> PathBuf {
    e.home.join("other-config")
}

fn plan_other(e: &Env) -> ServicePlan {
    plan_at(&other_config(e), &e.home, &e.binary, &e.vault, ID, false).unwrap()
}

fn other_unit(e: &Env) -> PathBuf {
    other_config(e).join("systemd/user").join(UNIT)
}

#[test]
fn second_setup_at_a_new_config_home_retires_the_first_unit() {
    let e = env();
    let fake = Fake::default();
    let wants = install_with_wants_link(&e, &fake);
    plan_other(&e).apply(&fake).unwrap();
    assert!(
        !unit_file(&e).exists(),
        "first unit survives the second setup"
    );
    assert!(std::fs::symlink_metadata(&wants).is_err());
    assert!(other_unit(&e).exists());
    assert!(fake.enabled.get() && fake.active.get());
    assert_eq!(recorded_units(&e.vault, UNIT).unwrap(), [other_unit(&e)]);
    assert!(uninstall_recorded_at(&other_config(&e), &e.vault, ID, &fake).unwrap());
    assert!(!other_unit(&e).exists());
    assert!(!fake.enabled.get() && !fake.active.get());
}

#[test]
fn failed_setup_at_a_new_config_home_keeps_the_previous_record() {
    let e = env();
    let fake = Fake::default();
    let wants = install_with_wants_link(&e, &fake);
    fake.fail.replace(vec!["daemon-reload"]);
    assert!(plan_other(&e).apply(&fake).is_err());
    assert!(!other_unit(&e).exists());
    // The failed apply keeps its intent: the new path stays recorded next to the old one.
    assert_eq!(
        recorded_units(&e.vault, UNIT).unwrap(),
        [unit_file(&e), other_unit(&e)]
    );
    assert!(uninstall_recorded_at(&other_config(&e), &e.vault, ID, &fake).unwrap());
    assert!(!unit_file(&e).exists());
    assert!(std::fs::symlink_metadata(&wants).is_err());
}

#[test]
fn failed_first_install_keeps_its_intent() {
    let e = env();
    let fake = Fake::failing(&["restart"]);
    assert!(do_plan(&e).apply(&fake).is_err());
    assert_eq!(recorded_units(&e.vault, UNIT).unwrap(), [unit_file(&e)]);
    // Uninstall with the record and no files present succeeds, then drops record and lock.
    uninstall_recorded_at(&other_config(&e), &e.vault, ID, &Fake::default()).unwrap();
    assert!(!record_path(&e.vault).exists());
    assert!(!lock_path(&e.vault, UNIT).exists());
}

#[test]
fn retiring_skips_a_recorded_path_that_is_no_longer_ours() {
    let e = env();
    let fake = Fake::default();
    do_plan(&e).apply(&fake).unwrap();
    let foreign = "[Service]\nExecStart=/bin/true\n";
    std::fs::write(unit_file(&e), foreign).unwrap();
    // A running unit is not ours once its recorded file is foreign.
    let err = plan_other(&e).apply(&fake).unwrap_err();
    assert!(err.to_string().contains("different service unit"), "{err}");
    fake.active.set(false);
    plan_other(&e).apply(&fake).unwrap();
    assert_eq!(std::fs::read_to_string(unit_file(&e)).unwrap(), foreign);
    assert_eq!(recorded_units(&e.vault, UNIT).unwrap(), [other_unit(&e)]);
}

#[test]
fn restart_failure_at_a_new_path_restarts_the_recorded_unit() {
    struct StopThenFail {
        fake: Fake,
        once: Cell<bool>,
    }
    impl Systemctl for StopThenFail {
        fn run(&self, args: &[&str]) -> std::io::Result<Output> {
            if args[0] == "restart" && !self.once.replace(true) {
                self.fake.calls.borrow_mut().push(args.join(" "));
                self.fake.active.set(false);
                return Ok(output(
                    1,
                    "restart stopped the old process, then start failed",
                ));
            }
            self.fake.run(args)
        }
    }
    let e = env();
    let fake = StopThenFail {
        fake: Fake::default(),
        once: Cell::new(true),
    };
    do_plan(&e).apply(&fake).unwrap();
    fake.once.set(false);
    assert!(plan_other(&e).apply(&fake).is_err());
    assert!(
        fake.fake.active.get(),
        "prior active unit was not restarted: {:?}",
        fake.fake.calls()
    );
    assert!(fake.fake.enabled.get());
    assert!(unit_file(&e).exists() && !other_unit(&e).exists());
    assert_eq!(
        recorded_units(&e.vault, UNIT).unwrap(),
        [unit_file(&e), other_unit(&e)]
    );
}

fn bad_records(e: &Env) -> usize {
    let dir = record_path(&e.vault).parent().unwrap().to_path_buf();
    std::fs::read_dir(dir)
        .unwrap()
        .filter(|entry| {
            let name = entry.as_ref().unwrap().file_name();
            name.to_string_lossy().starts_with("systemd-unit-path.bad-")
        })
        .count()
}

#[test]
fn malformed_record_is_set_aside_and_the_install_is_recorded() {
    let e = env();
    let fake = Fake::default();
    crate::install::local_dir(&e.vault).unwrap();
    std::fs::write(record_path(&e.vault), [0xff]).unwrap();
    do_plan(&e).apply(&fake).unwrap();
    assert!(unit_file(&e).exists() && fake.enabled.get() && fake.active.get());
    assert_eq!(recorded_units(&e.vault, UNIT).unwrap(), [unit_file(&e)]);
    assert_eq!(bad_records(&e), 1);
}

#[test]
fn malformed_record_with_a_failed_apply_leaves_no_unrecorded_unit() {
    let e = env();
    let fake = Fake::failing(&["restart"]);
    crate::install::local_dir(&e.vault).unwrap();
    std::fs::write(record_path(&e.vault), [0xff]).unwrap();
    assert!(do_plan(&e).apply(&fake).is_err());
    let recorded = recorded_units(&e.vault, UNIT).unwrap();
    assert!(!unit_file(&e).exists() || recorded.contains(&unit_file(&e)));
    assert!(!fake.enabled.get() && !fake.active.get());
}

/// Fake manager whose `restart` waits briefly for the other apply to reach the same point, so
/// unserialized applies interleave their record updates.
struct Rendezvous(std::sync::Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>);

impl Systemctl for Rendezvous {
    fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        match args[0] {
            "is-enabled" => Ok(answer(1, "disabled\n", "")),
            "is-active" => Ok(answer(3, "inactive\n", "")),
            "restart" | "start" => {
                let (count, ready) = &*self.0;
                let mut arrived = count.lock().unwrap();
                *arrived += 1;
                ready.notify_all();
                let wait = std::time::Duration::from_millis(100);
                drop(ready.wait_timeout_while(arrived, wait, |n| *n < 2).unwrap());
                Ok(output(0, ""))
            }
            _ => Ok(output(0, "")),
        }
    }
}

#[test]
fn concurrent_applies_at_two_config_homes_leave_no_unrecorded_unit() {
    for iteration in 0..20 {
        let e = env();
        let a = do_plan(&e);
        let b = plan_other(&e);
        crate::install::record_created_dirs(&e.vault, &[]).unwrap();
        let shared = std::sync::Arc::new(Default::default());
        let one = Rendezvous(std::sync::Arc::clone(&shared));
        let ta = std::thread::spawn(move || a.apply(&one));
        let tb = std::thread::spawn(move || b.apply(&Rendezvous(shared)));
        let (ra, rb) = (ta.join().unwrap(), tb.join().unwrap());
        assert!(ra.is_ok() && rb.is_ok(), "apply errors: {ra:?} {rb:?}");
        let recorded = recorded_units(&e.vault, UNIT).unwrap();
        for path in [unit_file(&e), other_unit(&e)] {
            assert!(
                !path.exists() || recorded.contains(&path),
                "iteration {iteration}: unrecorded unit {path:?}, record {recorded:?}"
            );
        }
    }
}

#[test]
fn uninstall_repairs_a_malformed_record_and_removes_the_lock_file() {
    let e = env();
    let fake = Fake::default();
    do_plan(&e).apply(&fake).unwrap();
    let lock = lock_path(&e.vault, UNIT);
    assert!(lock.exists());
    std::fs::write(record_path(&e.vault), [0xff]).unwrap();
    assert!(uninstall_recorded_at(&e.config, &e.vault, ID, &fake).unwrap());
    assert!(!unit_file(&e).exists() && !fake.enabled.get() && !fake.active.get());
    assert!(!record_path(&e.vault).exists() && !lock.exists());
    assert_eq!(bad_records(&e), 1);
}

#[test]
fn refused_apply_does_not_record_a_foreign_unit() {
    let e = env();
    let fake = Fake::default();
    let plan = do_plan(&e);
    write_foreign(&e);
    assert!(plan.apply(&fake).is_err());
    assert!(!record_path(&e.vault).exists());
}

#[test]
fn review3_failed_link_cleanup_keeps_intent() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    struct LeaveLink(PathBuf);
    impl Systemctl for LeaveLink {
        fn run(&self, args: &[&str]) -> std::io::Result<Output> {
            match args[0] {
                "is-enabled" => Ok(answer(1, "disabled\n", "")),
                "is-active" => Ok(answer(3, "inactive\n", "")),
                "restart" => {
                    let wants = self.0.parent().unwrap().join(WANTS_DIR);
                    std::fs::create_dir_all(&wants).unwrap();
                    symlink(&self.0, wants.join(UNIT)).unwrap();
                    std::fs::set_permissions(&wants, std::fs::Permissions::from_mode(0o555))
                        .unwrap();
                    Ok(output(1, "Failed to restart."))
                }
                _ => Ok(output(0, "")),
            }
        }
    }
    let e = env();
    let path = unit_file(&e);
    let result = do_plan(&e).apply(&LeaveLink(path.clone()));
    let wants = path.parent().unwrap().join(WANTS_DIR);
    std::fs::set_permissions(&wants, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_err());
    assert!(!path.exists());
    assert!(
        std::fs::symlink_metadata(wants.join(UNIT)).is_ok(),
        "fixture: link cleanup should have failed"
    );
    let kept = recorded_units(&e.vault, UNIT).unwrap();
    uninstall_recorded_at(&other_config(&e), &e.vault, ID, &Fake::default()).unwrap();
    assert!(
        std::fs::symlink_metadata(wants.join(UNIT)).is_err(),
        "uninstall left an unrecorded enablement link; record after failed apply: {kept:?}"
    );
}
