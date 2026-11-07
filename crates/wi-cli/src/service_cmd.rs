//! `wi service`: install, remove and inspect the per-vault LaunchAgent.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::json;
use wi_export::service::{self, SystemLaunchctl};

use super::{Output, ServiceCommand, Session};

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unset")
}

pub(crate) fn run(vault: &Session, command: ServiceCommand) -> Result<Output> {
    let home = home()?;
    let vault_id = vault.vault_id();
    match command {
        ServiceCommand::Install {
            yes,
            allow_dev_binary,
        } => {
            let binary =
                std::fs::canonicalize(std::env::current_exe()?).context("locate the wi binary")?;
            let root = std::fs::canonicalize(vault.root()).context("locate the vault")?;
            let plan =
                service::plan_with_options(&home, &binary, &root, vault_id, allow_dev_binary)?;
            let diff = plan.diff();
            if yes {
                plan.apply(&SystemLaunchctl)?;
            }
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
            Ok(Output {
                json: json!({
                    "applied": yes,
                    "changed": !plan.is_noop(),
                    "label": plan.label,
                    "plist_path": plan.plist_path,
                    "log_path": plan.log_path,
                    "diff": diff,
                }),
                human,
            })
        }
        ServiceCommand::Uninstall => {
            let removed = service::uninstall(&home, vault_id, &SystemLaunchctl)?;
            Ok(Output {
                json: json!({"removed": removed}),
                human: if removed {
                    "Service unloaded and its plist removed.".into()
                } else {
                    "Nothing to remove: the service is not installed.".into()
                },
            })
        }
        ServiceCommand::Status => {
            let status = service::status(&home, vault_id, &SystemLaunchctl)?;
            let human = format!(
                "{}: {}, {}\nplist: {}\nlog: {}",
                status.label,
                if status.installed {
                    "installed"
                } else {
                    "not installed"
                },
                if status.loaded {
                    "loaded"
                } else {
                    "not loaded"
                },
                status.plist_path.display(),
                status.log_path.display()
            );
            Ok(Output {
                json: serde_json::to_value(&status)?,
                human,
            })
        }
    }
}
