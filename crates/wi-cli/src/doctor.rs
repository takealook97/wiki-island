//! `wi doctor`: health checks in plain sentences.

use anyhow::Result;

use super::Output;
use super::service_cmd;
use wi_store::Vault;

pub(crate) fn doctor(vault: &Vault) -> Result<Output> {
    let mut report = vault.doctor()?;
    // Requests are durable ingress records; converted agent tasks are
    // reported separately rather than as stale owner warnings.
    report
        .checks
        .retain(|check| !matches!(check.name, "work" | "watcher"));
    let tasks = vault
        .work_list()?
        .iter()
        .filter(|w| w.actionable && w.state.as_deref() == Some("ready"))
        .count();
    report.checks.push(wi_store::Check {
        name: "work",
        status: "ok",
        detail: format!("{tasks} task(s) for your AI"),
        fix: None,
    });
    if let Some(check) = service_cmd::doctor_check(vault.vault_id()) {
        report.checks.push(check);
    }
    for check in &mut report.checks {
        if check.detail.contains("i/o error") || check.detail.contains("os error") {
            check.detail =
                "Some files could not be read. Automatic care will retry when they are available."
                    .into();
        }
        if check
            .fix
            .as_deref()
            .is_some_and(|fix| fix.contains("wi commit"))
        {
            check.fix = Some("Automatic care retries on the next command.".into());
        }
    }
    let mut human = format!("Notes: {}", report.root);
    for check in &report.checks {
        let (name, detail) = match (check.name, check.status) {
            ("git", "ok") => ("history", "Saved history is up to date."),
            ("manifest", "ok") => ("saves", "All saves are recorded."),
            ("manifest", _) => (
                "saves",
                "Some saves are waiting to finish; automatic recovery will retry.",
            ),
            ("index", "ok") => ("search", "Search is up to date."),
            ("index", _) => (
                "search",
                "Search is catching up automatically; notes remain safe.",
            ),
            ("temp-files", _) => (
                "temporary files",
                "Interrupted saves are being checked automatically.",
            ),
            _ => (check.name, check.detail.as_str()),
        };
        human.push_str(&format!("\n[{}] {}: {}", check.status, name, detail));
        if !matches!(check.name, "manifest" | "index" | "temp-files")
            && let Some(fix) = &check.fix
        {
            human.push_str(&format!("\n      Next: {fix}"));
        }
    }
    let mut json = serde_json::to_value(&report)?;
    json["work_requests"] = serde_json::to_value(vault.work_requests()?)?;
    json["index_degraded"] = serde_json::to_value(&vault.open_report().index_degraded)?;
    Ok(Output { json, human })
}
