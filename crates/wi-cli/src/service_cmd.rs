//! `wi service`: install, remove and inspect the per-vault background service
//! (a LaunchAgent on macOS, a systemd user unit on Linux).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;
use wi_export::service;

use super::{Output, ServiceCommand, Session};

fn home() -> Result<PathBuf> {
    super::setup_cmd::integration_home()
}

pub(crate) fn run(vault: &Session, command: ServiceCommand) -> Result<Output> {
    let home = home()?;
    let vault_id = vault.vault_id();
    let runner = service::default_runner();
    match command {
        ServiceCommand::Install {
            yes,
            allow_dev_binary,
        } => {
            ensure_manager(runner)?;
            let binary = binary()?;
            let root = std::fs::canonicalize(vault.root()).context("locate the vault")?;
            let plan =
                service::plan_with_options(&home, &binary, &root, vault_id, allow_dev_binary)?;
            if yes {
                plan.apply(runner)?;
            }
            Ok(install_output(&plan, yes))
        }
        ServiceCommand::Uninstall => {
            #[cfg(target_os = "linux")]
            let removed = service::uninstall_recorded(
                &home,
                &std::fs::canonicalize(vault.root()).context("locate the vault")?,
                vault_id,
                runner,
            )?;
            #[cfg(not(target_os = "linux"))]
            let removed = service::uninstall(&home, vault_id, runner)?;
            Ok(Output {
                json: json!({"removed": removed}),
                human: if !removed {
                    "Nothing to remove: the service is not installed.".into()
                } else if cfg!(target_os = "linux") {
                    "Service stopped and its unit removed.".into()
                } else {
                    "Service unloaded and its plist removed.".into()
                },
            })
        }
        ServiceCommand::Status => {
            ensure_manager(runner)?;
            let status = service::status(&home, vault_id, runner)?;
            Ok(Output {
                json: serde_json::to_value(&status)?,
                human: status_text(&status),
            })
        }
    }
}

/// Explicit service commands report a missing user manager as one plain sentence.
fn ensure_manager(runner: &service::Runner) -> Result<()> {
    #[cfg(target_os = "linux")]
    if !service::available(runner) {
        return Err(
            wi_export::install::Error::Unsupported(service::unavailable_message().into()).into(),
        );
    }
    let _ = runner;
    Ok(())
}

#[cfg(target_os = "linux")]
fn binary() -> Result<PathBuf> {
    super::setup_cmd::executable().context("locate the wi binary")
}

#[cfg(not(target_os = "linux"))]
fn binary() -> Result<PathBuf> {
    std::fs::canonicalize(std::env::current_exe()?).context("locate the wi binary")
}

fn yes_no(value: bool, yes: &'static str, no: &'static str) -> &'static str {
    if value { yes } else { no }
}

#[cfg(not(target_os = "linux"))]
fn install_output(plan: &service::ServicePlan, yes: bool) -> Output {
    let diff = plan.diff();
    let human = if yes {
        format!(
            "{diff}Installed {} and loaded it.\nplist: {}\nlog: {}",
            plan.label,
            plan.plist_path.display(),
            plan.log_path.display()
        )
    } else if plan.is_noop() {
        format!(
            "No change: {} already matches. Pass --yes to (re)load it.",
            plan.plist_path.display()
        )
    } else {
        format!("{diff}Preview only; pass --yes to apply.")
    };
    Output {
        json: json!({
            "applied": yes,
            "changed": !plan.is_noop(),
            "label": plan.label,
            "plist_path": plan.plist_path,
            "log_path": plan.log_path,
            "diff": diff,
        }),
        human,
    }
}

#[cfg(target_os = "linux")]
fn install_output(plan: &service::ServicePlan, yes: bool) -> Output {
    let diff = plan.diff();
    let human = if yes {
        format!(
            "{diff}Installed {} and started it.\nunit: {}",
            plan.label,
            plan.unit_path.display()
        )
    } else if plan.is_noop() {
        format!(
            "No change: {} already matches. Pass --yes to (re)start it.",
            plan.unit_path.display()
        )
    } else {
        format!("{diff}Preview only; pass --yes to apply.")
    };
    Output {
        json: json!({
            "applied": yes,
            "changed": !plan.is_noop(),
            "label": plan.label,
            "unit_path": plan.unit_path,
            "diff": diff,
        }),
        human,
    }
}

#[cfg(not(target_os = "linux"))]
fn status_text(status: &service::ServiceStatus) -> String {
    format!(
        "{}: {}, {}\nplist: {}\nlog: {}",
        status.label,
        yes_no(status.installed, "installed", "not installed"),
        yes_no(status.loaded, "loaded", "not loaded"),
        status.plist_path.display(),
        status.log_path.display()
    )
}

#[cfg(target_os = "linux")]
fn status_text(status: &service::ServiceStatus) -> String {
    format!(
        "{}: {}, {}, {}\nunit: {}",
        status.label,
        yes_no(status.installed, "installed", "not installed"),
        yes_no(status.enabled, "enabled", "not enabled"),
        yes_no(status.running, "running", "not running"),
        status.unit_path.display()
    )
}

/// Whether the service is set up and its watcher is running right now.
pub(crate) fn running(home: &Path, vault_id: &str, runner: &service::Runner) -> Result<bool> {
    let status = service::status(home, vault_id, runner)?;
    #[cfg(target_os = "linux")]
    let running = status.configured() && status.running;
    #[cfg(not(target_os = "linux"))]
    let running = status.loaded;
    Ok(running)
}

/// Whether this target has a service backend that can be used now.
pub(crate) fn ready(runner: &service::Runner) -> bool {
    #[cfg(target_os = "linux")]
    {
        service::available(runner)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = runner;
        cfg!(target_os = "macos")
    }
}

/// The doctor's service check, or `None` on a target without a service backend.
pub(crate) fn doctor_check(vault_id: &str) -> Option<wi_store::Check> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return None;
    }
    let runner = service::default_runner();
    if cfg!(target_os = "linux") && !ready(runner) {
        return Some(wi_store::Check {
            name: "service",
            status: "warn",
            detail: "Background care needs a systemd user session; until you log in to one, run wi watch to keep your notes up to date.".into(),
            fix: None,
        });
    }
    Some(
        match super::setup_cmd::home().and_then(|home| running(&home, vault_id, runner)) {
            Ok(running) => wi_store::Check {
                name: "service",
                status: if running { "ok" } else { "warn" },
                detail: if running {
                    "Background care is running."
                } else {
                    "Background care is not running."
                }
                .into(),
                fix: (!running).then(|| "run wi setup".into()),
            },
            Err(_) => wi_store::Check {
                name: "service",
                status: "warn",
                detail: "Background care could not be checked.".into(),
                fix: Some("run wi setup".into()),
            },
        },
    )
}

/// Trims the launchd log; systemd keeps the watcher's output in the journal. Returns false when
/// the log could not be trimmed.
pub(crate) fn rotate_log(home: &Path, vault_id: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        service::rotate_log(home, vault_id).is_ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (home, vault_id);
        true
    }
}
