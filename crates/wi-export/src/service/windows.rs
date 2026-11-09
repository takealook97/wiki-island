//! Task Scheduler logon task that keeps `wi watch` running for one vault (Windows).
//!
//! The task belongs to the current user: it starts at that user's logon with their own
//! interactive token and least privilege, so registering, starting and removing it needs no
//! administrator rights. Task Scheduler restarts a failed watcher every minute and never stops
//! it for running long or on battery.
use super::{absolute_str, describe, stable_binary, stderr_text, validate_vault_id};
use crate::install::Error;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;

const TASK_PREFIX: &str = "wiki-island-";
const SCHTASKS: &str = "schtasks.exe";
/// `SCHED_S_TASK_RUNNING`, the numeric last result of a running task in every display language.
const RUNNING_RESULT: &str = "267009";
const RUNNING_STATUS: &str = "Running";
/// Column positions in `schtasks /Query /FO CSV /V` rows, fixed across display languages.
const STATUS_COLUMN: usize = 3;
const LAST_RESULT_COLUMN: usize = 6;
const UNAVAILABLE: &str = "Background care needs Windows Task Scheduler, which is not available on this computer; run wi watch to keep this vault up to date";
const NO_USER: &str =
    "Windows did not report the current user; sign in again, then run wi setup again";

/// Runs `schtasks.exe` with the given arguments. Tests substitute a fake.
pub trait Schtasks {
    fn run(&self, args: &[&str]) -> std::io::Result<Output>;

    fn create(&self, xml_file: &str, task: &str) -> std::io::Result<Output> {
        self.run(&["/Create", "/XML", xml_file, "/TN", task, "/F"])
    }
    fn start(&self, task: &str) -> std::io::Result<Output> {
        self.run(&["/Run", "/TN", task])
    }
    fn end(&self, task: &str) -> std::io::Result<Output> {
        self.run(&["/End", "/TN", task])
    }
    fn delete(&self, task: &str) -> std::io::Result<Output> {
        self.run(&["/Delete", "/TN", task, "/F"])
    }
    fn export(&self, task: &str) -> std::io::Result<Output> {
        self.run(&["/Query", "/TN", task, "/XML"])
    }
    fn details(&self, task: &str) -> std::io::Result<Output> {
        self.run(&["/Query", "/TN", task, "/FO", "CSV", "/V", "/NH"])
    }
    fn list(&self) -> std::io::Result<Output> {
        self.run(&["/Query", "/FO", "CSV", "/NH"])
    }
}

/// Executes `WI_SCHTASKS`, or `schtasks.exe` from the system path when unset.
pub struct SystemSchtasks;

impl Schtasks for SystemSchtasks {
    fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        std::process::Command::new(
            std::env::var_os("WI_SCHTASKS").unwrap_or_else(|| SCHTASKS.into()),
        )
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
    }
}

/// True when Task Scheduler answers a query for this user's tasks.
pub fn available(schtasks: &dyn Schtasks) -> bool {
    succeeded(schtasks.list())
}

/// The sentence shown when Task Scheduler cannot be used.
pub fn unavailable_message() -> &'static str {
    UNAVAILABLE
}

#[derive(Debug)]
pub struct ServicePlan {
    /// Task name, `wiki-island-<vault-id>`.
    pub label: String,
    pub task_name: String,
    /// The task definition as registered.
    pub contents: String,
    /// The registered definition as Task Scheduler exports it, when the task exists.
    pub existing: Option<String>,
    marker: String,
}

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub label: String,
    pub installed: bool,
    pub running: bool,
    pub task_name: String,
}

impl ServiceStatus {
    /// The task is registered and starts at logon.
    pub fn configured(&self) -> bool {
        self.installed
    }
}

fn task_name(vault_id: &str) -> Result<String, Error> {
    validate_vault_id(vault_id)?;
    Ok(format!("{TASK_PREFIX}{vault_id}"))
}

/// The ownership line every task this backend registers carries in its description.
fn marker(vault_id: &str) -> String {
    format!("Managed by wiki-island. Vault {vault_id}.")
}

fn foreign(task: &str) -> Error {
    Error::Config(format!(
        "A scheduled task named {task} already exists and was not created by wiki-island; rename or remove it in Task Scheduler, then run wi setup again"
    ))
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Quotes one argument so the Windows command-line parser returns it unchanged.
fn quote_arg(arg: &str) -> String {
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

fn render_body(sid: &str, binary: &str, vault: &str, description: &str) -> String {
    let command = if binary.contains(' ') {
        format!("\"{binary}\"")
    } else {
        binary.to_owned()
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{description}</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{sid}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
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
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
      <WorkingDirectory>{vault}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        sid = xml_escape(sid),
        command = xml_escape(&command),
        arguments = xml_escape(&format!("watch --vault {}", quote_arg(vault))),
        vault = xml_escape(vault),
    )
}

/// Renders the task definition. The description carries the ownership marker and a revision
/// derived from the rest of the definition, so an exported task shows whether it still matches.
fn render(vault_id: &str, sid: &str, binary: &str, vault: &str) -> String {
    let marker = marker(vault_id);
    let revision = wi_lifecycle::sha256(render_body(sid, binary, vault, &marker).as_bytes());
    let description = format!("{marker} Revision {}.", &revision[7..23]);
    render_body(sid, binary, vault, &description)
}

fn description(xml: &str) -> Option<&str> {
    let start = xml.find("<Description>")? + "<Description>".len();
    let end = start + xml[start..].find("</Description>")?;
    Some(&xml[start..end])
}

/// Decodes `schtasks` output, which is UTF-16 with a byte-order mark or single-byte text.
fn decode(bytes: &[u8]) -> String {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => {
            let units: Vec<u16> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn succeeded(result: std::io::Result<Output>) -> bool {
    result.is_ok_and(|out| out.status.success())
}

fn check(result: std::io::Result<Output>, action: &str) -> Result<(), Error> {
    let out = result?;
    if out.status.success() {
        return Ok(());
    }
    let detail = stderr_text(&out);
    Err(Error::Config(if detail.is_empty() {
        format!(
            "Task Scheduler could not {action} the background task ({})",
            describe(&out)
        )
    } else {
        format!("Task Scheduler could not {action} the background task: {detail}")
    }))
}

/// The registered definition, or `None` when no task of that name exists.
fn exported(schtasks: &dyn Schtasks, task: &str) -> Result<Option<String>, Error> {
    let out = schtasks.export(task)?;
    Ok(out.status.success().then(|| decode(&out.stdout)))
}

/// Splits one CSV row with double-quoted fields.
fn csv_fields(row: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    fields.push(field);
    fields
}

fn is_running(schtasks: &dyn Schtasks, task: &str) -> Result<bool, Error> {
    let out = schtasks.details(task)?;
    if !out.status.success() {
        return Ok(false);
    }
    let text = decode(&out.stdout);
    Ok(text
        .lines()
        .filter(|row| !row.trim().is_empty())
        .any(|row| {
            let fields = csv_fields(row.trim());
            fields.get(STATUS_COLUMN).map(String::as_str) == Some(RUNNING_STATUS)
                || fields.get(LAST_RESULT_COLUMN).map(String::as_str) == Some(RUNNING_RESULT)
        }))
}

/// The current user's security identifier, from `whoami /user`.
fn current_user_sid() -> Result<String, Error> {
    let out = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|_| Error::Config(NO_USER.into()))?;
    let text = decode(&out.stdout);
    let sid = out
        .status
        .success()
        .then(|| csv_fields(text.trim()).into_iter().nth(1))
        .flatten()
        .filter(|sid| sid.starts_with("S-1-") && !sid.chars().any(char::is_whitespace));
    sid.ok_or_else(|| Error::Config(NO_USER.into()))
}

/// Plans the task for `vault`. Planning only reads Task Scheduler.
pub fn plan(
    home: &Path,
    wi_binary: &Path,
    vault: &Path,
    vault_id: &str,
) -> Result<ServicePlan, Error> {
    plan_with_options(home, wi_binary, vault, vault_id, false)
}

/// Like [`plan`]; `allow_dev_binary` accepts a binary inside a cargo build folder.
pub fn plan_with_options(
    home: &Path,
    wi_binary: &Path,
    vault: &Path,
    vault_id: &str,
    allow_dev_binary: bool,
) -> Result<ServicePlan, Error> {
    let task = task_name(vault_id)?;
    absolute_str(home, "home")?;
    let binary = stable_binary(wi_binary, allow_dev_binary)?;
    let vault_text = absolute_str(vault, "vault")?;
    let sid = current_user_sid()?;
    let existing = exported(&SystemSchtasks, &task)?;
    plan_at(vault_id, &sid, &plain(binary), &plain(vault_text), existing)
}

/// Drops the `\\?\` prefix that canonical Windows paths carry; Task Scheduler expects the plain
/// form for the program and working folder.
fn plain(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        path.to_owned()
    }
}

fn plan_at(
    vault_id: &str,
    sid: &str,
    binary: &str,
    vault: &str,
    existing: Option<String>,
) -> Result<ServicePlan, Error> {
    let task = task_name(vault_id)?;
    Ok(ServicePlan {
        label: task.clone(),
        task_name: task,
        contents: render(vault_id, sid, binary, vault),
        existing,
        marker: marker(vault_id),
    })
}

fn is_owned(xml: &str, marker: &str) -> bool {
    description(xml).is_some_and(|text| text.starts_with(marker))
}

/// Writes the definition as UTF-16 with a byte-order mark, the encoding `schtasks /XML` reads.
fn write_definition(contents: &str, task: &str) -> Result<PathBuf, Error> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("{task}-{}-{stamp}.xml", std::process::id()));
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(contents.encode_utf16().flat_map(u16::to_le_bytes));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(&bytes)?;
    Ok(path)
}

/// Registers `contents` under `task` from a temporary definition file.
fn register(schtasks: &dyn Schtasks, contents: &str, task: &str) -> Result<(), Error> {
    let file = write_definition(contents, task)?;
    let result = file
        .to_str()
        .ok_or_else(|| Error::Config("temporary folder must be valid UTF-8".into()))
        .and_then(|path| check(schtasks.create(path, task), "register"));
    let _ = std::fs::remove_file(&file);
    result
}

impl ServicePlan {
    /// Unified diff of the registered definition against the planned one.
    pub fn diff(&self) -> String {
        if self.is_noop() {
            return String::new();
        }
        let old = self.existing.as_deref().unwrap_or("");
        super::plan_diff(Path::new(&self.task_name), old, &self.contents)
    }

    /// True when the registered task already carries this definition's revision.
    pub fn is_noop(&self) -> bool {
        let planned = description(&self.contents);
        self.existing
            .as_deref()
            .is_some_and(|xml| planned.is_some() && description(xml) == planned)
    }

    /// Registers the task and starts the watcher now. On failure the previous registration and
    /// running state are restored, or the new task is removed.
    pub fn apply(&self, schtasks: &dyn Schtasks) -> Result<(), Error> {
        let task = &self.task_name;
        let current = exported(schtasks, task)?;
        if current
            .as_deref()
            .is_some_and(|xml| !is_owned(xml, &self.marker))
        {
            return Err(foreign(task));
        }
        let was_running = current.is_some() && is_running(schtasks, task)?;
        let unchanged = current.as_deref().is_some_and(|xml| {
            description(xml).is_some() && description(xml) == description(&self.contents)
        });
        if unchanged && was_running {
            return Ok(());
        }
        let result = self.activate(schtasks, unchanged, was_running);
        if let Err(error) = result {
            self.rollback(schtasks, current.as_deref(), was_running);
            return Err(error);
        }
        Ok(())
    }

    fn activate(
        &self,
        schtasks: &dyn Schtasks,
        unchanged: bool,
        was_running: bool,
    ) -> Result<(), Error> {
        let task = &self.task_name;
        if !unchanged {
            register(schtasks, &self.contents, task)?;
        }
        // A running watcher keeps its old program and arguments until it is restarted.
        if was_running {
            let _ = schtasks.end(task);
        }
        check(schtasks.start(task), "start")
    }

    fn rollback(&self, schtasks: &dyn Schtasks, previous: Option<&str>, was_running: bool) {
        let task = &self.task_name;
        let _ = schtasks.end(task);
        match previous {
            Some(xml) => {
                if register(schtasks, xml, task).is_ok() && was_running {
                    let _ = schtasks.start(task);
                }
            }
            None => {
                let _ = schtasks.delete(task);
            }
        }
    }
}

/// Stops the watcher and removes the task. Returns false when there was nothing to remove.
pub fn uninstall(home: &Path, vault_id: &str, schtasks: &dyn Schtasks) -> Result<bool, Error> {
    let _ = home;
    let task = task_name(vault_id)?;
    let Some(current) = exported(schtasks, &task)? else {
        return Ok(false);
    };
    if !is_owned(&current, &marker(vault_id)) {
        return Err(foreign(&task));
    }
    // Ending a task that is not running fails harmlessly.
    let _ = schtasks.end(&task);
    check(schtasks.delete(&task), "remove")?;
    Ok(true)
}

/// Reports whether the task is registered and whether its watcher runs now.
pub fn status(
    home: &Path,
    vault_id: &str,
    schtasks: &dyn Schtasks,
) -> Result<ServiceStatus, Error> {
    let _ = home;
    let task = task_name(vault_id)?;
    let installed = exported(schtasks, &task)?.is_some();
    let running = installed && is_running(schtasks, &task)?;
    Ok(ServiceStatus {
        label: task.clone(),
        installed,
        running,
        task_name: task,
    })
}

#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;
