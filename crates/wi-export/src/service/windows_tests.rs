use super::*;
use std::cell::RefCell;

const SID: &str = "S-1-5-21-1000-2000-3000-1001";
const BINARY: &str = r"C:\Users\Ann Lee\.cargo\bin\wi.exe";
const VAULT: &str = r#"C:\Users\Ann Lee\Notes & <Ideas> "x" 노트\"#;

fn output(code: i32, stdout: &[u8]) -> Output {
    #[cfg(unix)]
    let status = std::os::unix::process::ExitStatusExt::from_raw(code << 8);
    #[cfg(windows)]
    let status = std::os::windows::process::ExitStatusExt::from_raw(code as u32);
    Output {
        status,
        stdout: stdout.to_vec(),
        stderr: if code == 0 {
            Vec::new()
        } else {
            b"ERROR: refused".to_vec()
        },
    }
}

/// An in-memory Task Scheduler with one task slot. `fail` names a verb that fails once.
#[derive(Default)]
struct Fake {
    xml: RefCell<Option<String>>,
    running: RefCell<bool>,
    unavailable: bool,
    fail: RefCell<Option<&'static str>>,
    calls: RefCell<Vec<String>>,
}

impl Fake {
    fn with_task(xml: &str, running: bool) -> Self {
        let fake = Self::default();
        *fake.xml.borrow_mut() = Some(xml.to_owned());
        *fake.running.borrow_mut() = running;
        fake
    }
    fn failing(self, verb: &'static str) -> Self {
        *self.fail.borrow_mut() = Some(verb);
        self
    }
    fn verbs(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Schtasks for Fake {
    fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        let verb = match args {
            ["/Query", "/TN", _, "/XML"] => "export",
            ["/Query", "/TN", _, ..] => "details",
            ["/Query", ..] => "list",
            [first, ..] => first.trim_start_matches('/'),
            [] => "none",
        };
        self.calls.borrow_mut().push(verb.to_owned());
        if self.unavailable {
            return Ok(output(1, b""));
        }
        if *self.fail.borrow() == Some(verb) {
            *self.fail.borrow_mut() = None;
            return Ok(output(1, b""));
        }
        let exists = self.xml.borrow().is_some();
        Ok(match verb {
            "list" => match self.xml.borrow().as_deref() {
                Some(_) => output(0, b"\"\\wiki-island-v1\",\"N/A\",\"Ready\"\r\n"),
                None => output(0, b"\"\\Other\",\"N/A\",\"Ready\"\r\n"),
            },
            "export" => match self.xml.borrow().as_deref() {
                Some(xml) => output(0, xml.as_bytes()),
                None => output(1, b""),
            },
            "details" if exists => {
                let status = if *self.running.borrow() {
                    "Running"
                } else {
                    "Ready"
                };
                let row = format!(
                    "\"PC\",\"\\{}\",\"N/A\",\"{status}\",\"Interactive only\"\r\n",
                    args[2]
                );
                output(0, row.as_bytes())
            }
            "Create" => {
                let bytes = std::fs::read(args[2]).unwrap();
                assert_eq!(&bytes[..2], &[0xFF, 0xFE], "definition must be UTF-16");
                *self.xml.borrow_mut() = Some(decode(&bytes));
                output(0, b"")
            }
            "Run" if exists => {
                *self.running.borrow_mut() = true;
                output(0, b"")
            }
            "End" if exists && *self.running.borrow() => {
                *self.running.borrow_mut() = false;
                output(0, b"")
            }
            "Delete" if exists => {
                *self.xml.borrow_mut() = None;
                *self.running.borrow_mut() = false;
                output(0, b"")
            }
            _ => output(1, b""),
        })
    }
}

fn planned(existing: Option<String>) -> ServicePlan {
    plan_at("v1", SID, BINARY, VAULT, existing).unwrap()
}

#[test]
fn renders_exact_task_definition_with_escaped_paths() {
    let xml = render("v1", SID, BINARY, VAULT);
    let revision = revision(&xml).unwrap();
    assert_eq!(revision.len(), 16);
    let expected = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Source>wiki-island</Source>
    <Version>{revision}</Version>
    <Description>Managed by wiki-island. Vault v1.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>S-1-5-21-1000-2000-3000-1001</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-21-1000-2000-3000-1001</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Hidden>true</Hidden>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>&quot;C:\Users\Ann Lee\.cargo\bin\wi.exe&quot;</Command>
      <Arguments>watch --vault &quot;C:\Users\Ann Lee\Notes &amp; &lt;Ideas&gt; \&quot;x\&quot; 노트\\&quot;</Arguments>
      <WorkingDirectory>C:\Users\Ann Lee\Notes &amp; &lt;Ideas&gt; &quot;x&quot; 노트\</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    );
    assert_eq!(xml, expected);
}

#[test]
fn revision_changes_with_the_definition() {
    let one = render("v1", SID, BINARY, VAULT);
    let other = render("v1", SID, BINARY, r"C:\Notes");
    assert_ne!(revision(&one), revision(&other));
    assert_eq!(render("v1", SID, BINARY, VAULT), one);
}

#[test]
fn quotes_arguments_for_the_windows_parser() {
    assert_eq!(quote_arg(r"C:\a b"), r#""C:\a b""#);
    assert_eq!(quote_arg(r"C:\dir\"), r#""C:\dir\\""#);
    assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
    assert_eq!(quote_arg(""), r#""""#);
}

#[test]
fn binary_without_spaces_is_not_quoted() {
    let xml = render("v1", SID, r"C:\bin\wi.exe", r"C:\Notes");
    assert!(xml.contains(r"<Command>C:\bin\wi.exe</Command>"), "{xml}");
    assert!(xml.contains(r"<Arguments>watch --vault &quot;C:\Notes&quot;</Arguments>"));
}

#[test]
fn decodes_utf16_and_single_byte_output() {
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(
        "<Description>노트</Description>"
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    assert_eq!(decode(&bytes), "<Description>노트</Description>");
    assert_eq!(decode(b"plain"), "plain");
}

#[test]
fn running_is_read_from_status_or_numeric_last_result() {
    let fields = csv_fields(r#""PC","\t","N/A","Wird ausgeführt","x","y","267009""#);
    assert_eq!(fields[LAST_RESULT_COLUMN], RUNNING_RESULT);
    assert_eq!(csv_fields(r#""a ""b""",c"#), vec![r#"a "b""#, "c"]);

    struct Localized(&'static str);
    impl Schtasks for Localized {
        fn run(&self, _: &[&str]) -> std::io::Result<Output> {
            Ok(output(0, self.0.as_bytes()))
        }
    }
    let german = Localized(r#""PC","\wiki-island-v1","N/A","Wird ausgeführt","I","t","267009""#);
    assert!(is_running(&german, "wiki-island-v1").unwrap());
    let ready = Localized(r#""PC","\wiki-island-v1","N/A","Bereit","I","t","0""#);
    assert!(!is_running(&ready, "wiki-island-v1").unwrap());
}

#[test]
fn install_registers_and_starts_the_task() {
    let fake = Fake::default();
    let plan = planned(None);
    assert!(!plan.is_noop());
    assert!(
        plan.diff()
            .contains("+      <LogonType>InteractiveToken</LogonType>")
    );
    plan.apply(&fake).unwrap();
    assert_eq!(fake.xml.borrow().as_deref(), Some(plan.contents.as_str()));
    assert!(*fake.running.borrow());
    assert_eq!(fake.verbs(), ["list", "Create", "Run"]);
    let status = status(Path::new(r"C:\Users\Ann"), "v1", &fake).unwrap();
    assert!(status.configured() && status.running);
    assert_eq!(status.task_name, "wiki-island-v1");
}

#[test]
fn matching_running_task_is_a_noop() {
    let xml = render("v1", SID, BINARY, VAULT);
    let fake = Fake::with_task(&xml, true);
    let plan = planned(Some(xml));
    assert!(plan.is_noop());
    assert_eq!(plan.diff(), "");
    plan.apply(&fake).unwrap();
    assert_eq!(fake.verbs(), ["list", "export", "details"]);
}

#[test]
fn matching_stopped_task_is_started_without_registering() {
    let xml = render("v1", SID, BINARY, VAULT);
    let fake = Fake::with_task(&xml, false);
    planned(Some(xml)).apply(&fake).unwrap();
    assert_eq!(fake.verbs(), ["list", "export", "details", "Run"]);
    assert!(*fake.running.borrow());
}

#[test]
fn changed_running_task_is_replaced_and_restarted() {
    let old = render("v1", SID, BINARY, r"C:\Old");
    let fake = Fake::with_task(&old, true);
    let plan = planned(Some(old));
    plan.apply(&fake).unwrap();
    assert_eq!(
        fake.verbs(),
        [
            "list", "export", "details", "End", "details", "Create", "Run"
        ]
    );
    assert_eq!(fake.xml.borrow().as_deref(), Some(plan.contents.as_str()));
    assert!(*fake.running.borrow());
}

#[test]
fn failed_start_removes_a_new_task() {
    let fake = Fake::default().failing("Run");
    let error = planned(None).apply(&fake).unwrap_err();
    assert!(error.to_string().contains("could not start"), "{error}");
    assert!(fake.xml.borrow().is_none());
    // The start never happened, so nothing is ended; only the registration is undone.
    assert_eq!(fake.verbs(), ["list", "Create", "Run", "Delete"]);
}

#[test]
fn failed_start_restores_the_previous_task_and_running_state() {
    let old = render("v1", SID, BINARY, r"C:\Old");
    // The first /Run (the new definition) fails; the restore's /Run succeeds.
    let fake = Fake::with_task(&old, true).failing("Run");
    assert!(planned(Some(old.clone())).apply(&fake).is_err());
    assert_eq!(fake.xml.borrow().as_deref(), Some(old.as_str()));
    assert!(*fake.running.borrow());
}

#[test]
fn failed_registration_keeps_a_new_task_absent() {
    let fake = Fake::default().failing("Create");
    let error = planned(None).apply(&fake).unwrap_err();
    assert!(error.to_string().contains("could not register"), "{error}");
    assert!(fake.xml.borrow().is_none());
}

#[test]
fn foreign_task_is_never_replaced_or_removed() {
    let foreign_xml =
        "<Task><RegistrationInfo><Description>Backup job</Description></RegistrationInfo></Task>";
    let fake = Fake::with_task(foreign_xml, true);
    let error = planned(Some(foreign_xml.into())).apply(&fake).unwrap_err();
    assert!(
        error.to_string().contains("not created by wiki-island"),
        "{error}"
    );
    assert!(uninstall(Path::new(r"C:\Users\Ann"), "v1", &fake).is_err());
    assert_eq!(fake.xml.borrow().as_deref(), Some(foreign_xml));
    assert_eq!(fake.verbs(), ["list", "export", "list", "export"]);
    let state = status(Path::new(r"C:\Users\Ann"), "v1", &fake).unwrap();
    assert!(state.foreign && !state.configured() && !state.running);
}

#[test]
fn uninstall_stops_and_deletes_and_is_idempotent() {
    let xml = render("v1", SID, BINARY, VAULT);
    let fake = Fake::with_task(&xml, true);
    let home = Path::new(r"C:\Users\Ann");
    assert!(uninstall(home, "v1", &fake).unwrap());
    assert!(fake.xml.borrow().is_none() && !*fake.running.borrow());
    assert!(!uninstall(home, "v1", &fake).unwrap());
    assert_eq!(
        fake.verbs(),
        [
            "list", "export", "details", "End", "details", "Delete", "list"
        ]
    );
}

#[test]
fn uninstall_of_a_stopped_task_does_not_end_it() {
    let xml = render("v1", SID, BINARY, VAULT);
    let fake = Fake::with_task(&xml, false);
    assert!(uninstall(Path::new(r"C:\Users\Ann"), "v1", &fake).unwrap());
    assert!(fake.xml.borrow().is_none());
    assert!(!fake.verbs().iter().any(|v| v == "End"));
}

#[test]
fn status_distinguishes_configured_from_running() {
    let home = Path::new(r"C:\Users\Ann");
    let absent = status(home, "v1", &Fake::default()).unwrap();
    assert!(!absent.configured() && !absent.running);
    let xml = render("v1", SID, BINARY, VAULT);
    let stopped = status(home, "v1", &Fake::with_task(&xml, false)).unwrap();
    assert!(stopped.configured() && !stopped.running);
}

#[test]
fn availability_follows_the_task_list_query() {
    assert!(available(&Fake::default()));
    let off = Fake {
        unavailable: true,
        ..Fake::default()
    };
    assert!(!available(&off));
    assert!(unavailable_message().contains("run wi watch"));
}

#[test]
fn invalid_vault_id_is_rejected() {
    assert!(plan_at("bad id", SID, BINARY, VAULT, None).is_err());
    assert!(status(Path::new(r"C:\Users\Ann"), "../x", &Fake::default()).is_err());
}

#[test]
fn canonical_prefixes_are_dropped() {
    assert_eq!(plain(r"\\?\C:\Users\Ann"), r"C:\Users\Ann");
    assert_eq!(plain(r"\\?\UNC\server\share\x"), r"\\server\share\x");
    assert_eq!(plain(r"C:\Notes"), r"C:\Notes");
}

/// Models the documented scheduler behavior the in-memory fake leaves out: under IgnoreNew a
/// `/Run` while an instance runs keeps the old instance, and `/Delete` never stops a running
/// instance. `failures` lists verbs that fail once each; `stuck` makes `/End` report success
/// without stopping anything.
struct Scheduler {
    fake: Fake,
    failures: RefCell<Vec<&'static str>>,
    active: RefCell<Option<String>>,
    stuck: bool,
}

impl Scheduler {
    fn new(xml: Option<&str>, running: bool, failures: &[&'static str]) -> Self {
        Self {
            fake: xml.map_or_else(Fake::default, |x| Fake::with_task(x, running)),
            failures: RefCell::new(failures.to_vec()),
            active: RefCell::new(xml.filter(|_| running).map(str::to_owned)),
            stuck: false,
        }
    }
    fn definition(&self) -> Option<String> {
        self.fake.xml.borrow().clone()
    }
    fn process(&self) -> Option<String> {
        self.active.borrow().clone()
    }
}

impl Schtasks for Scheduler {
    fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        let verb = match args {
            ["/Query", "/TN", _, "/XML"] => "export",
            ["/Query", "/TN", _, ..] => "details",
            ["/Query", ..] => "list",
            [first, ..] => first.trim_start_matches('/'),
            [] => "none",
        };
        let failed = {
            let mut failures = self.failures.borrow_mut();
            let index = failures.iter().position(|x| *x == verb);
            index.map(|index| failures.remove(index)).is_some()
        };
        if failed {
            self.fake.calls.borrow_mut().push(verb.to_owned());
            return Ok(output(1, b""));
        }
        if verb == "Run" && self.active.borrow().is_some() {
            self.fake.calls.borrow_mut().push(verb.to_owned());
            return Ok(output(0, b""));
        }
        if verb == "End" && self.stuck {
            self.fake.calls.borrow_mut().push(verb.to_owned());
            return Ok(output(0, b""));
        }
        let before = self.process();
        let out = self.fake.run(args)?;
        if out.status.success() {
            match verb {
                "Run" => *self.active.borrow_mut() = self.definition(),
                "End" => *self.active.borrow_mut() = None,
                "Delete" => {
                    *self.fake.running.borrow_mut() = before.is_some();
                    *self.active.borrow_mut() = before;
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

const FOREIGN: &str =
    "<Task><RegistrationInfo><Description>Backup</Description></RegistrationInfo></Task>";

#[test]
fn failed_export_never_overwrites_an_existing_task() {
    let fake = Scheduler::new(Some(FOREIGN), true, &["export"]);
    assert!(planned(None).apply(&fake).is_err());
    assert_eq!(fake.definition().as_deref(), Some(FOREIGN));
    assert_eq!(fake.process().as_deref(), Some(FOREIGN));
    assert!(!fake.fake.verbs().iter().any(|v| v == "Create"));
}

#[test]
fn failed_task_list_changes_nothing() {
    let old = render("v1", SID, BINARY, r"C:\Old");
    for fail in ["list", "export"] {
        let fake = Scheduler::new(Some(&old), true, &[fail]);
        assert!(planned(None).apply(&fake).is_err(), "{fail}");
        assert_eq!(fake.definition().as_deref(), Some(old.as_str()));
        assert_eq!(fake.process().as_deref(), Some(old.as_str()));
        let mutations = ["Create", "Run", "End", "Delete"];
        assert!(
            !fake
                .fake
                .verbs()
                .iter()
                .any(|v| mutations.contains(&v.as_str())),
            "{fail}: {:?}",
            fake.fake.verbs()
        );
    }
}

#[test]
fn failed_query_is_never_reported_as_nothing_to_remove() {
    let old = render("v1", SID, BINARY, VAULT);
    for fail in ["list", "export"] {
        let fake = Scheduler::new(Some(&old), true, &[fail]);
        assert!(uninstall(Path::new("/tmp"), "v1", &fake).is_err(), "{fail}");
        assert!(fake.definition().is_some());
        assert!(fake.process().is_some());
    }
}

#[test]
fn failed_query_is_never_reported_as_not_configured() {
    let old = render("v1", SID, BINARY, VAULT);
    for fail in ["list", "export", "details"] {
        let fake = Scheduler::new(Some(&old), true, &[fail]);
        assert!(status(Path::new("/tmp"), "v1", &fake).is_err(), "{fail}");
    }
}

#[test]
fn failed_running_query_aborts_before_registering() {
    let old = render("v1", SID, BINARY, r"C:\Old");
    let fake = Scheduler::new(Some(&old), true, &["details"]);
    assert!(planned(Some(old.clone())).apply(&fake).is_err());
    assert_eq!(fake.definition().as_deref(), Some(old.as_str()));
    assert_eq!(fake.process().as_deref(), Some(old.as_str()));
    assert!(!fake.fake.verbs().iter().any(|v| v == "Create"));
}

#[test]
fn failed_end_keeps_the_old_watcher_and_definition() {
    let old = render("v1", SID, BINARY, r"C:\Old");
    let fake = Scheduler::new(Some(&old), true, &["End"]);
    assert!(planned(Some(old.clone())).apply(&fake).is_err());
    assert_eq!(fake.definition().as_deref(), Some(old.as_str()));
    assert_eq!(fake.process().as_deref(), Some(old.as_str()));
    assert!(!fake.fake.verbs().iter().any(|v| v == "Create"));
}

#[test]
fn watcher_that_does_not_stop_aborts_the_update() {
    let old = render("v1", SID, BINARY, r"C:\Old");
    let fake = Scheduler {
        stuck: true,
        ..Scheduler::new(Some(&old), true, &[])
    };
    assert!(planned(Some(old.clone())).apply(&fake).is_err());
    assert_eq!(fake.definition().as_deref(), Some(old.as_str()));
    assert_eq!(fake.process().as_deref(), Some(old.as_str()));
    assert!(!fake.fake.verbs().iter().any(|v| v == "Create"));
}

#[test]
fn changed_definition_replaces_the_running_watcher() {
    let old = render("v1", SID, BINARY, r"C:\Old");
    let fake = Scheduler::new(Some(&old), true, &[]);
    let plan = planned(Some(old));
    plan.apply(&fake).unwrap();
    assert_eq!(fake.definition().as_deref(), Some(plan.contents.as_str()));
    assert_eq!(fake.process().as_deref(), Some(plan.contents.as_str()));
}

#[test]
fn failed_uninstall_end_keeps_the_task_for_a_retry() {
    let old = render("v1", SID, BINARY, VAULT);
    let fake = Scheduler::new(Some(&old), true, &["End"]);
    let error = uninstall(Path::new("/tmp"), "v1", &fake).unwrap_err();
    assert!(error.to_string().contains("kept"), "{error}");
    assert_eq!(fake.definition().as_deref(), Some(old.as_str()));
    assert_eq!(fake.process().as_deref(), Some(old.as_str()));
}

#[test]
fn uninstall_stops_the_watcher_before_deleting() {
    let old = render("v1", SID, BINARY, VAULT);
    let fake = Scheduler::new(Some(&old), true, &[]);
    assert!(uninstall(Path::new("/tmp"), "v1", &fake).unwrap());
    assert!(fake.definition().is_none());
    assert!(fake.process().is_none());
}

#[test]
fn failed_registration_restarts_the_watcher_it_stopped() {
    let old = render("v1", SID, BINARY, r"C:\Old");
    let fake = Scheduler::new(Some(&old), true, &["Create", "Create"]);
    assert!(planned(Some(old.clone())).apply(&fake).is_err());
    assert_eq!(fake.definition().as_deref(), Some(old.as_str()));
    assert_eq!(fake.process().as_deref(), Some(old.as_str()));
}

#[test]
fn failed_delete_keeps_the_registration() {
    let old = render("v1", SID, BINARY, VAULT);
    let fake = Scheduler::new(Some(&old), true, &["Delete"]);
    assert!(uninstall(Path::new("/tmp"), "v1", &fake).is_err());
    assert!(fake.definition().is_some());
}

#[test]
fn foreign_task_is_not_reported_as_background_care() {
    let fake = Fake::with_task(FOREIGN, true);
    let state = status(Path::new("/tmp"), "v1", &fake).unwrap();
    assert!(!state.configured() && !state.running);
}

#[test]
fn task_without_the_source_marker_is_foreign() {
    let owned = render("v1", SID, BINARY, VAULT);
    let unmarked = owned.replace("<Source>wiki-island</Source>", "");
    let fake = Fake::with_task(&unmarked, false);
    assert!(planned(None).apply(&fake).is_err());
    assert_eq!(fake.xml.borrow().as_deref(), Some(unmarked.as_str()));
    let other_vault = Fake::with_task(&render("v2", SID, BINARY, VAULT), false);
    assert!(uninstall(Path::new("/tmp"), "v1", &other_vault).is_err());
}

#[test]
fn percent_in_a_path_is_rejected_at_plan_time() {
    let binary = plan_at("v1", SID, r"C:\%USERNAME%\wi.exe", r"C:\Notes", None).unwrap_err();
    assert!(binary.to_string().contains('%'), "{binary}");
    let vault = plan_at("v1", SID, BINARY, r"C:\notes\%USERNAME%", None).unwrap_err();
    assert!(vault.to_string().contains('%'), "{vault}");
}

#[test]
fn task_with_the_earlier_marker_is_ours_and_updated() {
    let plan = planned(None);
    let earlier = plan
        .contents
        .replace("    <Source>wiki-island</Source>\n", "")
        .replace(&format!("    <Version>{}</Version>\n", revision(&plan.contents).unwrap()), "")
        .replace(
            "<Description>Managed by wiki-island. Vault v1.</Description>",
            "<Description>Managed by wiki-island. Vault v1. Revision 0123456789abcdef.</Description>",
        );
    assert_ne!(earlier, plan.contents);
    let fake = Fake::with_task(&earlier, true);
    plan.apply(&fake).unwrap();
    assert_eq!(fake.xml.borrow().as_deref(), Some(plan.contents.as_str()));
    let near = earlier.replace("0123456789abcdef.", "0123456789abcdef. Backup");
    assert!(!is_owned(&near, &marker("v1")));
}
