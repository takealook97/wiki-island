//! Unattended local setup and ownership-scoped removal.
use super::Output;
use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use wi_export::{hosts, install, service, skills};
use wi_store::Vault;

/// Preserve the invocation path of a package-manager symlink in a stable bin folder.
pub(crate) fn executable() -> Result<PathBuf> {
    if let Some(arg) = std::env::args_os().next() {
        let invoked = PathBuf::from(arg);
        if invoked.components().count() > 1 {
            let absolute = std::path::absolute(&invoked)?;
            if absolute.is_file() {
                return Ok(absolute);
            }
        } else if let Some(path) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&path) {
                let candidate = directory.join(&invoked);
                if candidate.is_file() {
                    return Ok(std::path::absolute(candidate)?);
                }
            }
        }
    }
    Ok(std::env::current_exe()?)
}

pub(crate) fn home() -> Result<PathBuf> {
    #[cfg(windows)]
    let home = {
        // Empty or relative values are unset: they would place the vault
        // and app configurations under the working directory.
        let set = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
        let absolute =
            |value: std::ffi::OsString| Some(value).filter(|v| Path::new(v).is_absolute());
        set("USERPROFILE").and_then(absolute).or_else(|| {
            let mut drive = set("HOMEDRIVE")?;
            drive.push(set("HOMEPATH")?);
            absolute(drive)
        })
    };
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from)
        .context("Your home directory could not be located.")
}
/// Locate the home used by explicit app and service commands.
pub(crate) fn integration_home() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        home()
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is unset")
    }
}
pub(crate) fn root(root: &Option<PathBuf>, home: &Path) -> PathBuf {
    root.clone().unwrap_or_else(|| home.join("wiki-island"))
}

/// First maintenance pass: take in existing files, then run the shared maintenance pass.
fn run_first_maintenance(vault: &Vault) -> Result<()> {
    vault.auto_ingest()?;
    vault.maintain(&wi_store::MaintainOptions::default())?;
    vault.flush()?;
    Ok(())
}

pub(crate) fn setup(
    root: &Path,
    home: &Path,
    binary: &Path,
    codex_home: Option<&Path>,
    runner: &service::Runner,
    primary: bool,
) -> Result<Output> {
    setup_with_options(
        root,
        home,
        binary,
        codex_home,
        runner,
        wi_store::OpenOptions::default(),
        primary,
    ).map_err(|_| anyhow::anyhow!("Setup could not finish; check that wiki-island is installed in its permanent location and your AI app settings are readable and writable, then run wi setup again."))
}
fn setup_with_options(
    root: &Path,
    home: &Path,
    binary: &Path,
    codex_home: Option<&Path>,
    runner: &service::Runner,
    options: wi_store::OpenOptions,
    primary: bool,
) -> Result<Output> {
    let home = std::path::absolute(home)?;
    let home = home.as_path();
    let absolute = std::path::absolute(root)?;
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        missing.push(
            ancestor
                .file_name()
                .context("The notes folder could not be located.")?,
        );
        ancestor = ancestor
            .parent()
            .context("The notes folder could not be located.")?;
    }
    let mut root = std::fs::canonicalize(ancestor)?;
    for component in missing.into_iter().rev() {
        root.push(component);
    }
    let mut detected = hosts::detect(home);
    if codex_home.is_some_and(|path| !path.is_dir()) {
        detected.retain(|host| *host != hosts::HostId::Codex);
    }
    if codex_home.is_some_and(Path::is_dir) && !detected.contains(&hosts::HostId::Codex) {
        detected.push(hosts::HostId::Codex);
    }
    // Without a usable service manager the service is skipped, not an error.
    let background_ready = super::service_cmd::ready(runner);
    // Validate every host and service destination before creating the vault or editing a host.
    let preflight = || -> Result<Vec<install::Plan>> {
        if !binary.is_file() {
            anyhow::bail!("The installed program could not be found.");
        }
        if background_ready {
            let existing_id = std::fs::read_to_string(root.join(".wiki-island/config.toml"))
                .ok()
                .map(|text| wi_note::VaultConfig::parse(&text))
                .transpose()?;
            let id = existing_id
                .as_ref()
                .and_then(|config| config.vault_id())
                .unwrap_or("preflight");
            service::plan(home, binary, &root, id)?;
        }
        let plans = detected
            .iter()
            .map(|host| {
                let plan = install::plan_host(*host, home, codex_home, binary, &root)?;
                plan.preflight()?;
                Ok(plan)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(plans)
    };
    let plans = preflight().map_err(|_| anyhow::anyhow!(
        "Setup could not start; install the program in its permanent location and check that your AI app settings are readable and writable."
    ))?;
    // Inside a file-synced folder the vault starts in folder mode (history
    // in this device's data folder); otherwise in git mode.
    let (vault, _) = Vault::init_with(&root, options)?;
    if primary {
        vault.set_primary_device()?;
    }
    let storage = match vault.storage_mode() {
        wi_store::StorageMode::Git => "",
        wi_store::StorageMode::Folder if vault.is_primary_device() => {
            "History is kept on this device, outside the synced folder; this device runs note maintenance.\n"
        }
        wi_store::StorageMode::Folder => {
            "History is kept on this device, outside the synced folder; another device runs note maintenance (wi setup --primary takes over).\n"
        }
    };
    let targets = detected
        .iter()
        .filter_map(|host| skills::SkillTarget::for_host(*host))
        .collect::<std::collections::BTreeSet<_>>();
    wi_export::local::Targets {
        home: home.to_owned(),
        hosts: detected
            .iter()
            .zip(&plans)
            .map(|(host, plan)| (*host, plan.path.clone()))
            .collect(),
        skill_dirs: targets.iter().map(|target| target.root(home)).collect(),
        cache_root: {
            let cache = cache_root(home)?;
            std::fs::canonicalize(&cache).or_else(|_| std::path::absolute(cache))?
        },
    }
    .save(&root)?;
    let background_plan = if background_ready {
        Some(service::plan(home, binary, &root, vault.vault_id())?)
    } else {
        None
    };
    let background_on = match background_plan {
        Some(plan) => match start_background(&plan, home, vault.vault_id(), runner) {
            Ok(()) => true,
            // The manager went away after preflight: skip like a host without one.
            Err(_) if manager_lost(runner) => false,
            Err(error) => {
                install::cleanup_created_dirs(&root)?;
                return Err(error);
            }
        },
        None => false,
    };
    let background = if background_on {
        "Background care is on."
    } else if cfg!(target_os = "linux") {
        "Your notes are ready; background care needs a systemd user session, so run wi watch to keep them up to date."
    } else if cfg!(windows) {
        "Your notes are ready; background care needs Windows Task Scheduler, so run wi watch to keep them up to date."
    } else {
        "Your notes are ready; background care is available on macOS."
    };
    let mut targets = Vec::new();
    for (host, plan) in detected.iter().zip(plans) {
        plan.apply()?;
        targets.extend(skills::SkillTarget::for_host(*host));
    }
    skills::enable(&vault, &targets)?;
    run_first_maintenance(&vault)?;
    let synced = skills::sync(&vault, home, &skills::configured(&vault), true)?;
    let skill_message = if !synced.skipped.is_empty() {
        format!(
            "{} skill location(s) could not be updated.",
            synced.skipped.len()
        )
    } else if !synced.renamed.is_empty() {
        format!(
            "Skills are ready; {} use alternative names to preserve your skills.",
            synced.renamed.len()
        )
    } else {
        "Skills are up to date.".into()
    };
    Ok(Output {
        human: format!(
            "Your notes are ready at {}.\n{storage}Connected {} AI app(s). {skill_message}\n{background}",
            root.display(),
            detected.len()
        ),
        json: json!({"vault":root,"storage":vault.storage_mode(),"primary":vault.is_primary_device(),"hosts":detected,"skills":synced,"background":background_on}),
    })
}
pub(crate) fn uninstall(
    root: &Path,
    _home: &Path,
    _codex_home: Option<&Path>,
    runner: &service::Runner,
) -> Result<Output> {
    if !root.join(".wiki-island/config.toml").is_file() {
        return Ok(Output {
            human: "Nothing to remove. Your notes are safe.".into(),
            json: json!({"removed":false}),
        });
    }
    // Read identity without opening the store: uninstall must not replay or ingest notes.
    let config = wi_note::VaultConfig::parse(&std::fs::read_to_string(
        root.join(".wiki-island/config.toml"),
    )?)?;
    let vault_id = config
        .vault_id()
        .context("Your notes folder has no wiki-island identity.")?;
    let root = std::fs::canonicalize(root)?;
    let state = wi_export::local::Targets::load(&root)?;
    let Some(state) = state else {
        return Ok(Output {
            human: "No recorded app connections to remove. Your notes are safe.".into(),
            json: json!({"removed":false,"notes_kept":true}),
        });
    };
    #[cfg(target_os = "linux")]
    service::uninstall_recorded(&state.home, &root, vault_id, runner)?;
    #[cfg(any(target_os = "macos", windows))]
    service::uninstall(&state.home, vault_id, runner)?;
    // Other targets have no service to remove.
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let _ = runner;
    let mut removed = 0;
    for (host, path) in &state.hosts {
        if let Some(plan) = install::plan_remove_at(*host, path, &root)? {
            plan.apply()?;
            removed += 1;
        }
    }
    let skills = skills::uninstall_recorded(vault_id, &state)?;
    skills::disable_at(&root)?;
    install::cleanup_created_dirs(&root)?;
    remove_cache(&state.cache_root, vault_id)?;
    Ok(Output {
        human: "wiki-island is disconnected and background care is removed.".into(),
        json: json!({"removed":true,"hosts":removed,"skills":skills,"notes_kept":true}),
    })
}

/// Applies the service plan unless it already matches and runs.
fn start_background(
    plan: &service::ServicePlan,
    home: &Path,
    vault_id: &str,
    runner: &service::Runner,
) -> Result<()> {
    if !plan.is_noop() || !super::service_cmd::running(home, vault_id, runner)? {
        plan.apply(runner)?;
    }
    Ok(())
}

/// True when the target's service manager is no longer reachable.
fn manager_lost(runner: &service::Runner) -> bool {
    cfg!(any(target_os = "linux", windows)) && !super::service_cmd::ready(runner)
}

fn cache_root(home: &Path) -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("WI_CACHE_DIR") {
        return Ok(PathBuf::from(root));
    }
    #[cfg(windows)]
    {
        let _ = home;
        // Reuse the index's Windows known-folder resolver.
        let path = wi_index::Index::cache_path("setup")?;
        path.parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .context("Your cache directory could not be located.")
    }
    #[cfg(not(windows))]
    {
        Ok(if cfg!(target_os = "macos") {
            home.join("Library/Caches/wiki-island")
        } else {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".cache"))
                .join("wiki-island")
        })
    }
}

fn remove_cache(root: &Path, vault_id: &str) -> Result<()> {
    anyhow::ensure!(
        !vault_id.is_empty()
            && vault_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Invalid notes folder identity."
    );
    // A changed cache ancestor must not redirect recursive cleanup.
    for ancestor in root.ancestors() {
        if std::fs::symlink_metadata(ancestor).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Ok(());
        }
    }
    let cache = root.join(vault_id);
    match std::fs::symlink_metadata(&cache) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            std::fs::remove_dir_all(cache)?
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::{
        cell::Cell,
        process::{ExitStatus, Output as ProcessOutput},
    };
    struct FakeLaunchctl {
        calls: Cell<usize>,
        loaded: Cell<bool>,
    }
    // A Linux host without a user manager: setup and removal skip the service.
    #[cfg(target_os = "linux")]
    impl service::Systemctl for FakeLaunchctl {
        fn run(&self, args: &[&str]) -> std::io::Result<ProcessOutput> {
            self.calls.set(self.calls.get() + 1);
            assert!(!self.loaded.get());
            assert_eq!(args, ["show-environment"], "unexpected service operation");
            Ok(ProcessOutput {
                status: ExitStatus::from_raw(1 << 8),
                stdout: vec![],
                stderr: vec![],
            })
        }
    }
    #[cfg(not(target_os = "linux"))]
    impl service::Launchctl for FakeLaunchctl {
        fn run(&self, args: &[&str]) -> std::io::Result<ProcessOutput> {
            self.calls.set(self.calls.get() + 1);
            let success = match args[0] {
                "print" => self.loaded.get(),
                "bootstrap" => {
                    self.loaded.set(true);
                    true
                }
                "bootout" => self.loaded.replace(false),
                other => panic!("unexpected service operation: {other}"),
            };
            Ok(ProcessOutput {
                status: ExitStatus::from_raw(if success { 0 } else { 3 << 8 }),
                stdout: vec![],
                stderr: vec![],
            })
        }
    }
    #[test]
    fn setup_and_uninstall_are_unattended_idempotent_and_keep_notes() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        for host in hosts::HOSTS {
            std::fs::create_dir_all(home.join(host.detect_path)).unwrap();
        }
        let root = home.join("wiki-island");
        let binary = temp.path().join("bin/wi");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "fake installed executable").unwrap();
        let fake = FakeLaunchctl {
            calls: Cell::new(0),
            loaded: Cell::new(false),
        };
        let options = || wi_store::OpenOptions {
            index_path: Some(temp.path().join("cache.sqlite3")),
        };
        let out = setup_with_options(&root, &home, &binary, None, &fake, options(), false).unwrap();
        assert_eq!(out.human.lines().count(), 3);
        // Skills land only under the given home, one owned copy per target.
        let owned = |dir: &str| {
            std::fs::read_dir(home.join(dir))
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .filter(|e| e.path().join(".wi-owned").is_file())
                        .count()
                })
                .unwrap_or(0)
        };
        assert!(owned(".claude/skills") >= 1 && owned(".agents/skills") >= 1);
        assert_eq!(
            out.json["hosts"].as_array().unwrap().len(),
            hosts::HOSTS.len()
        );
        let id = Vault::open_with(&root, options())
            .unwrap()
            .vault_id()
            .to_owned();
        let note = root.join("keep.md");
        std::fs::write(&note, "# Keep this note\n").unwrap();
        let before_configs = hosts::HOSTS
            .iter()
            .map(|h| std::fs::read(home.join(h.config_path)).unwrap())
            .collect::<Vec<_>>();
        setup_with_options(&root, &home, &binary, None, &fake, options(), false).unwrap();
        assert_eq!(Vault::open_with(&root, options()).unwrap().vault_id(), id);
        assert_eq!(
            std::fs::read_to_string(&note).unwrap(),
            "# Keep this note\n"
        );
        for (host, before) in hosts::HOSTS.iter().zip(before_configs) {
            assert_eq!(std::fs::read(home.join(host.config_path)).unwrap(), before);
        }
        let out = uninstall(&root, &home, None, &fake).unwrap();
        assert_eq!(owned(".claude/skills") + owned(".agents/skills"), 0);
        assert_eq!(out.json["hosts"], hosts::HOSTS.len());
        assert!(out.json["notes_kept"].as_bool().unwrap());
        assert_eq!(
            std::fs::read_to_string(&note).unwrap(),
            "# Keep this note\n"
        );
        assert!(root.join(".git").is_dir());
        for host in hosts::HOSTS {
            assert!(
                !std::fs::read_to_string(home.join(host.config_path))
                    .unwrap_or_default()
                    .contains("wiki-island")
            );
        }
        assert_eq!(
            uninstall(&root, &home, None, &fake).unwrap().json["hosts"],
            0
        );
        assert_eq!(super::root(&None, &home), root);
    }
}

#[cfg(all(test, windows))]
mod windows_locations {
    use super::*;

    #[test]
    fn cache_uses_the_index_known_folder_resolver() {
        // An override takes precedence in both the index and setup.
        if let Some(root) = std::env::var_os("WI_CACHE_DIR") {
            assert_eq!(
                cache_root(Path::new("unused")).unwrap(),
                PathBuf::from(root)
            );
        } else {
            let database = wi_index::Index::cache_path("setup").unwrap();
            assert_eq!(
                cache_root(Path::new("unused")).unwrap(),
                database.parent().unwrap().parent().unwrap()
            );
        }
    }
}
