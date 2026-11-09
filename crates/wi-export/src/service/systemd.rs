//! systemd user unit that keeps `wi watch` running for one vault (Linux).
//!
//! The unit lives in `$XDG_CONFIG_HOME/systemd/user` (default `~/.config/systemd/user`) and is
//! started by the user's service manager. Nothing enables lingering, so the watcher runs while the
//! user manager runs, normally while the owner is logged in.
use super::{
    absolute_str, describe, ensure_writable, plan_diff, stable_binary, stderr_text, unique_backup,
    validate_vault_id,
};
use crate::install::Error;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;

const UNIT_PREFIX: &str = "wiki-island-";
const SYSTEMCTL: &str = "systemctl";
const WANTS_DIR: &str = "default.target.wants";
/// `systemctl` exit code for a unit that is not loaded or not installed.
const NOT_LOADED_CODE: i32 = 5;
const UNAVAILABLE: &str =
    "Background care needs a systemd user session; run wi watch to keep this vault up to date";

/// Runs `systemctl --user` with the given arguments. Tests substitute a fake.
pub trait Systemctl {
    fn run(&self, args: &[&str]) -> std::io::Result<Output>;

    fn show_environment(&self) -> std::io::Result<Output> {
        self.run(&["show-environment"])
    }
    fn daemon_reload(&self) -> std::io::Result<Output> {
        self.run(&["daemon-reload"])
    }
    fn enable(&self, unit: &str) -> std::io::Result<Output> {
        self.run(&["enable", unit])
    }
    fn disable(&self, unit: &str) -> std::io::Result<Output> {
        self.run(&["disable", unit])
    }
    fn disable_now(&self, unit: &str) -> std::io::Result<Output> {
        self.run(&["disable", "--now", unit])
    }
    fn start(&self, unit: &str) -> std::io::Result<Output> {
        self.run(&["start", unit])
    }
    fn stop(&self, unit: &str) -> std::io::Result<Output> {
        self.run(&["stop", unit])
    }
    fn restart(&self, unit: &str) -> std::io::Result<Output> {
        self.run(&["restart", unit])
    }
    fn is_enabled(&self, unit: &str) -> std::io::Result<Output> {
        self.run(&["is-enabled", unit])
    }
    fn is_active(&self, unit: &str) -> std::io::Result<Output> {
        self.run(&["is-active", unit])
    }
}

/// Executes `WI_SYSTEMCTL --user`, or `systemctl --user` from PATH when unset.
pub struct SystemSystemctl;

impl Systemctl for SystemSystemctl {
    fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        std::process::Command::new(
            std::env::var_os("WI_SYSTEMCTL").unwrap_or_else(|| SYSTEMCTL.into()),
        )
        .arg("--user")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
    }
}

/// True when a user service manager answers. Its environment is never shown.
pub fn available(systemctl: &dyn Systemctl) -> bool {
    succeeded(systemctl.show_environment())
}

#[derive(Debug)]
pub struct ServicePlan {
    /// Unit name, `wiki-island-<vault-id>.service`.
    pub label: String,
    pub unit_path: PathBuf,
    pub contents: String,
    pub existing: Option<String>,
    vault: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub label: String,
    pub installed: bool,
    pub enabled: bool,
    pub running: bool,
    pub unit_path: PathBuf,
}

impl ServiceStatus {
    /// The unit file exists and starts with the user session.
    pub fn configured(&self) -> bool {
        self.installed && self.enabled
    }
}

fn config_home(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
}

fn unit_path(config_home: &Path, vault_id: &str) -> Result<(String, PathBuf), Error> {
    validate_vault_id(vault_id)?;
    if !config_home.is_absolute() {
        return Err(Error::Config("home must be an absolute path".into()));
    }
    let label = format!("{UNIT_PREFIX}{vault_id}.service");
    let path = config_home.join("systemd/user").join(&label);
    Ok((label, path))
}

fn read_file(path: &Path) -> Result<Option<String>, Error> {
    super::read_file(path, "service unit")
}

/// One ExecStart word: quoted, with `\` and `"` escaped and `$` and `%` doubled so systemd
/// passes the path literally.
fn exec_word(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("$$"),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One Environment= assignment: quoted as a whole, `%` doubled; `$` is literal there.
fn env_assignment(name: &str, value: &str) -> String {
    let mut out = format!("\"{name}=");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn render(binary: &str, vault: &str, home: &str) -> String {
    format!(
        "[Unit]\nDescription=wiki-island watcher\n\n[Service]\nExecStart={} {} {} {}\nRestart=always\nRestartSec=5\nEnvironment={}\n\n[Install]\nWantedBy=default.target\n",
        exec_word(binary),
        exec_word("watch"),
        exec_word("--vault"),
        exec_word(vault),
        env_assignment("HOME", home),
    )
}

pub fn plan(
    home: &Path,
    wi_binary: &Path,
    vault_root: &Path,
    vault_id: &str,
) -> Result<ServicePlan, Error> {
    plan_with_options(home, wi_binary, vault_root, vault_id, false)
}

/// Allow development binary paths only when explicitly requested by the caller.
pub fn plan_with_options(
    home: &Path,
    wi_binary: &Path,
    vault_root: &Path,
    vault_id: &str,
    allow_dev_binary: bool,
) -> Result<ServicePlan, Error> {
    plan_at(
        &config_home(home),
        home,
        wi_binary,
        vault_root,
        vault_id,
        allow_dev_binary,
    )
}

fn plan_at(
    config_home: &Path,
    home: &Path,
    wi_binary: &Path,
    vault_root: &Path,
    vault_id: &str,
    allow_dev_binary: bool,
) -> Result<ServicePlan, Error> {
    let home_text = absolute_str(home, "home")?;
    let (label, unit_path) = unit_path(config_home, vault_id)?;
    crate::install::preflight_local(vault_root)?;
    let binary = stable_binary(wi_binary, allow_dev_binary)?;
    let vault = absolute_str(vault_root, "vault root")?;
    let existing = read_file(&unit_path)?;
    ensure_writable(unit_path.parent())?;
    Ok(ServicePlan {
        contents: render(binary, vault, home_text),
        label,
        unit_path,
        existing,
        vault: vault_root.to_path_buf(),
    })
}

fn succeeded(result: std::io::Result<Output>) -> bool {
    matches!(result, Ok(out) if out.status.success())
}

fn is_not_loaded(out: &Output) -> bool {
    let err = stderr_text(out).to_ascii_lowercase();
    out.status.code() == Some(NOT_LOADED_CODE)
        || err.contains("not loaded")
        || err.contains("does not exist")
        || err.contains("not found")
}

/// Turns a systemctl result into an error naming the operation.
fn check(
    result: std::io::Result<Output>,
    what: &str,
    tolerate_not_loaded: bool,
) -> Result<(), Error> {
    match result {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) if tolerate_not_loaded && is_not_loaded(&out) => Ok(()),
        Ok(out) => Err(Error::Service(format!(
            "systemctl --user {what} failed ({})",
            describe(&out)
        ))),
        Err(error) => Err(Error::Io(error)),
    }
}

fn write_atomic(parent: &Path, path: &Path, contents: &str) -> Result<(), Error> {
    let durability = wi_history::durable::Durability::Standard;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(contents.as_bytes())?;
    wi_history::durable::barrier(temp.as_file(), durability)?;
    temp.persist(path).map_err(|e| Error::Io(e.error))?;
    Ok(())
}

impl ServicePlan {
    /// Line diff between the existing unit and the proposed one; empty when identical.
    pub fn diff(&self) -> String {
        if self.is_noop() {
            return String::new();
        }
        plan_diff(
            &self.unit_path,
            self.existing.as_deref().unwrap_or(""),
            &self.contents,
        )
    }

    /// True when the unit bytes already match. Says nothing about whether it runs.
    pub fn is_noop(&self) -> bool {
        self.existing.as_deref() == Some(self.contents.as_str())
    }

    fn parent(&self) -> Result<&Path, Error> {
        self.unit_path
            .parent()
            .ok_or_else(|| Error::Config("unit has no parent".into()))
    }

    /// Writes, enables and (re)starts the unit. Identical content that is already enabled and
    /// active is left alone. Any failure restores the previous file, enablement and running state.
    pub fn apply(&self, systemctl: &dyn Systemctl) -> Result<(), Error> {
        let parent = self.parent()?;
        let durability = wi_history::durable::Durability::Standard;
        crate::install::record_created_dirs(
            &self.vault,
            &[parent.to_path_buf(), parent.join(WANTS_DIR)],
        )?;
        wi_history::durable::create_dir_durable(parent, durability)?;
        // Validate the live destination before any systemctl call, noop or not.
        if read_file(&self.unit_path)? != self.existing {
            return Err(Error::Stale);
        }
        let was_enabled = succeeded(systemctl.is_enabled(&self.label));
        let was_active = succeeded(systemctl.is_active(&self.label));
        let changed = !self.is_noop();
        if !changed && was_enabled && was_active {
            return Ok(());
        }
        if changed {
            if let Some(old) = &self.existing {
                unique_backup(&self.vault, &self.unit_path, old.as_bytes())?;
                wi_history::durable::barrier_dir(parent, durability)?;
            }
            if read_file(&self.unit_path)? != self.existing {
                return Err(Error::Stale);
            }
            write_atomic(parent, &self.unit_path, &self.contents)?;
            wi_history::durable::barrier_dir(parent, durability)?;
        }
        let Err(failure) = self.activate(systemctl, changed || was_active) else {
            return Ok(());
        };
        match self.rollback(systemctl, changed, was_enabled, was_active) {
            Ok(()) => Err(failure),
            Err(error) => Err(Error::Service(format!(
                "{failure}; restoring the previous service failed: {error}"
            ))),
        }
    }

    fn activate(&self, systemctl: &dyn Systemctl, restart: bool) -> Result<(), Error> {
        check(systemctl.daemon_reload(), "daemon-reload", false)?;
        check(systemctl.enable(&self.label), "enable", false)?;
        if restart {
            check(systemctl.restart(&self.label), "restart", false)
        } else {
            check(systemctl.start(&self.label), "start", false)
        }
    }

    /// Restores the previous file, enablement and running state; reports every failed step.
    fn rollback(
        &self,
        systemctl: &dyn Systemctl,
        changed: bool,
        was_enabled: bool,
        was_active: bool,
    ) -> Result<(), Error> {
        let unit = self.label.as_str();
        let mut steps = Vec::new();
        if self.existing.is_none() {
            // Stop and disable while the unit file still exists.
            steps.push(check(systemctl.stop(unit), "stop", true));
            steps.push(check(systemctl.disable(unit), "disable", true));
            if changed {
                steps.push(self.restore());
            }
            steps.push(check(systemctl.daemon_reload(), "daemon-reload", false));
        } else {
            if changed {
                steps.push(self.restore());
            }
            steps.push(check(systemctl.daemon_reload(), "daemon-reload", false));
            steps.push(if was_enabled {
                check(systemctl.enable(unit), "enable", false)
            } else {
                check(systemctl.disable(unit), "disable", true)
            });
            steps.push(if was_active {
                check(systemctl.restart(unit), "restart", false)
            } else {
                check(systemctl.stop(unit), "stop", true)
            });
        }
        let errors: Vec<String> = steps
            .into_iter()
            .filter_map(|step| step.err().map(|e| e.to_string()))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Service(errors.join("; ")))
        }
    }

    /// Restore the previewed bytes atomically; never overwrite a concurrent edit.
    fn restore(&self) -> Result<(), Error> {
        if read_file(&self.unit_path)?.as_deref() != Some(self.contents.as_str()) {
            return Err(Error::Stale);
        }
        let parent = self.parent()?;
        match &self.existing {
            Some(old) => write_atomic(parent, &self.unit_path, old)?,
            None => std::fs::remove_file(&self.unit_path)?,
        }
        wi_history::durable::barrier_dir(parent, wi_history::durable::Durability::Standard)?;
        Ok(())
    }
}

/// Stops, disables and removes the unit. Returns whether a unit file was removed. Without a
/// user manager it removes the unit file and its enablement link so nothing starts at next login.
pub fn uninstall(home: &Path, vault_id: &str, systemctl: &dyn Systemctl) -> Result<bool, Error> {
    uninstall_at(&config_home(home), vault_id, systemctl)
}

fn uninstall_at(
    config_home: &Path,
    vault_id: &str,
    systemctl: &dyn Systemctl,
) -> Result<bool, Error> {
    let (label, path) = unit_path(config_home, vault_id)?;
    let present = read_file(&path)?.is_some();
    if !available(systemctl) {
        if let Some(link) = path.parent().map(|dir| dir.join(WANTS_DIR).join(&label))
            && std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink())
        {
            std::fs::remove_file(link)?;
        }
        if present {
            std::fs::remove_file(&path)?;
        }
        return Ok(present);
    }
    // Always stop: a loaded unit can outlive a deleted file.
    if present {
        check(systemctl.disable_now(&label), "disable --now", true)?;
        std::fs::remove_file(&path)?;
    } else {
        check(systemctl.stop(&label), "stop", true)?;
    }
    check(systemctl.daemon_reload(), "daemon-reload", false)?;
    Ok(present)
}

pub fn status(
    home: &Path,
    vault_id: &str,
    systemctl: &dyn Systemctl,
) -> Result<ServiceStatus, Error> {
    status_at(&config_home(home), vault_id, systemctl)
}

fn status_at(
    config_home: &Path,
    vault_id: &str,
    systemctl: &dyn Systemctl,
) -> Result<ServiceStatus, Error> {
    let (label, unit_path) = unit_path(config_home, vault_id)?;
    if !available(systemctl) {
        return Err(Error::Unsupported(UNAVAILABLE.into()));
    }
    Ok(ServiceStatus {
        installed: read_file(&unit_path)?.is_some(),
        enabled: succeeded(systemctl.is_enabled(&label)),
        running: succeeded(systemctl.is_active(&label)),
        label,
        unit_path,
    })
}

/// The plain sentence shown when no user manager is reachable.
pub fn unavailable_message() -> &'static str {
    UNAVAILABLE
}

#[cfg(all(test, unix))]
#[path = "systemd_tests.rs"]
mod tests;
