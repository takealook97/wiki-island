//! `wi doctor`: health checks in plain sentences.

use anyhow::Result;

use super::Output;
use super::service_cmd;
use wi_export::local::Naming;
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
    if let Some(check) = apps_check(vault) {
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

/// The vault name, its app entry name and the recorded apps whose config no
/// longer has this vault's entry. `None` when this device records no apps.
fn apps_check(vault: &Vault) -> Option<wi_store::Check> {
    let targets = wi_export::local::Targets::load(vault).ok()??;
    // Entry names resolve as setup and uninstall resolve them; a recorded name this program
    // does not write is never shown or compared.
    let recorded = wi_export::local::Naming::load(vault).ok().flatten();
    let Some(naming) = recorded.as_ref().map_or(Some(Naming::default()), |_| {
        super::setup_name::recorded(vault.root())
    }) else {
        return Some(unsupported_name_check(vault.root()));
    };
    let missing: Vec<&str> = targets
        .hosts
        .iter()
        .filter(|(host, path)| {
            !wi_export::install::entries(*host, path)
                .iter()
                .any(|(name, served)| {
                    *name == naming.entry && served.as_deref() == Some(vault.root())
                })
        })
        .map(|(host, _)| host.spec().name)
        .collect();
    Some(apps_check_from(&naming, &missing, vault.root()))
}

/// The recorded app name is outside the form `wi setup` writes; setup chooses it again.
fn unsupported_name_check(root: &std::path::Path) -> wi_store::Check {
    wi_store::Check {
        name: "apps",
        status: "warn",
        detail: "The recorded AI app name is not one wi writes, so wi cannot tell which name AI apps use for this vault.".into(),
        fix: Some(format!("run {}", super::setup_cmd::setup_command_for(root))),
    }
}

fn apps_check_from(naming: &Naming, missing: &[&str], root: &std::path::Path) -> wi_store::Check {
    let mut detail = match &naming.name {
        Some(name) => format!(
            "This vault is named \"{name}\"; AI apps reach it as {}.",
            naming.entry
        ),
        None => format!(
            "This is the default vault; AI apps reach it as {}.",
            naming.entry
        ),
    };
    if !missing.is_empty() {
        detail.push_str(&format!(
            " These apps no longer have it: {}.",
            missing.join(", ")
        ));
    }
    wi_store::Check {
        name: "apps",
        status: if missing.is_empty() { "ok" } else { "warn" },
        detail,
        fix: (!missing.is_empty())
            .then(|| format!("run {}", super::setup_cmd::setup_command_for(root))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apps_check_names_the_vault_entry_and_missing_apps() {
        let root = std::path::Path::new("/notes/work");
        let default = apps_check_from(&wi_export::local::Naming::default(), &[], root);
        assert_eq!(default.status, "ok");
        assert_eq!(
            default.detail,
            "This is the default vault; AI apps reach it as wiki-island."
        );
        assert!(default.fix.is_none());
        let naming = wi_export::local::Naming {
            name: Some("work".into()),
            entry: "wiki-island-work".into(),
        };
        let named = apps_check_from(&naming, &["Codex", "Cursor"], root);
        assert_eq!(named.status, "warn");
        assert_eq!(
            named.detail,
            "This vault is named \"work\"; AI apps reach it as wiki-island-work. These apps no longer have it: Codex, Cursor."
        );
        assert!(named.fix.unwrap().starts_with("run wi setup"));
    }
}
