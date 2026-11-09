//! Per-vault background service that keeps `wi watch` running.
//!
//! The backend is chosen at compile time: launchd on macOS, a systemd user unit on Linux. Other
//! targets report the service as unsupported. Planning is read-only. Applying mirrors the host
//! installer protections: a symlink or non-regular destination is rejected, a stale preview
//! fails, a differing file is backed up under a unique name, and the replacement is atomic.
use crate::install::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;

#[cfg(not(target_os = "linux"))]
mod launchd;
// Compiled for unit tests on every Unix host so the systemd logic is exercised off Linux too.
#[cfg(any(all(test, unix), target_os = "linux"))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod systemd;

#[cfg(not(target_os = "linux"))]
pub use launchd::{
    Launchctl, ServicePlan, ServiceStatus, SystemLaunchctl, plan, plan_with_options, rotate_log,
    status, uninstall,
};
#[cfg(target_os = "linux")]
pub use systemd::{
    ServicePlan, ServiceStatus, SystemSystemctl, Systemctl, available, plan, plan_with_options,
    status, unavailable_message, uninstall,
};

/// The command runner of this target's backend.
#[cfg(not(target_os = "linux"))]
pub type Runner = dyn Launchctl;
/// The command runner of this target's backend.
#[cfg(target_os = "linux")]
pub type Runner = dyn Systemctl;

/// The process runner of this target's backend.
pub fn default_runner() -> &'static Runner {
    #[cfg(not(target_os = "linux"))]
    {
        &SystemLaunchctl
    }
    #[cfg(target_os = "linux")]
    {
        &SystemSystemctl
    }
}

fn validate_vault_id(vault_id: &str) -> Result<(), Error> {
    if vault_id.is_empty()
        || !vault_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Error::Config(format!(
            "invalid vault id {vault_id:?}; use only A-Z, a-z, 0-9, '_' and '-'"
        )));
    }
    Ok(())
}

/// Reads a service definition; `what` names it in the error for a symlink or non-regular file.
fn read_file(path: &Path, what: &str) -> Result<Option<String>, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.is_file() || m.file_type().is_symlink() => Err(Error::Config(format!(
            "{what} must be a regular file, not a symlink"
        ))),
        Ok(_) => String::from_utf8(std::fs::read(path)?)
            .map(Some)
            .map_err(|e| Error::Config(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn absolute_str<'a>(path: &'a Path, what: &str) -> Result<&'a str, Error> {
    if !path.is_absolute() {
        return Err(Error::Config(format!("{what} must be an absolute path")));
    }
    let text = path
        .to_str()
        .ok_or_else(|| Error::Config(format!("{what} must be valid UTF-8")))?;
    if text.chars().any(char::is_control) {
        return Err(Error::Config(format!(
            "{what} must not contain control characters"
        )));
    }
    Ok(text)
}

// Cargo can place artifacts in a custom cache or under a target triple. Directory
// names alone do not distinguish an installed binary from a development build.
fn is_dev_binary(path: &Path) -> bool {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical.ancestors().skip(1).take(4).any(|dir| {
        dir.join(".rustc_info.json").is_file()
            || std::fs::read_to_string(dir.join("CACHEDIR.TAG"))
                .is_ok_and(|tag| tag.to_ascii_lowercase().contains("cargo"))
    })
}

/// Validates the binary path, refusing a development build unless the caller allows it.
fn stable_binary(wi_binary: &Path, allow_dev_binary: bool) -> Result<&str, Error> {
    let binary = absolute_str(wi_binary, "wi binary")?;
    if !allow_dev_binary && is_dev_binary(wi_binary) {
        return Err(Error::Config("service needs a stable binary path; run `cargo install --path crates/wi-cli` or explicitly pass --allow-dev-binary".into()));
    }
    Ok(binary)
}

/// Fails when the nearest existing ancestor of any directory is not a writable directory.
fn ensure_writable<'a>(directories: impl IntoIterator<Item = &'a Path>) -> Result<(), Error> {
    for directory in directories {
        for ancestor in directory.ancestors() {
            match std::fs::metadata(ancestor) {
                Ok(meta) if !meta.is_dir() || meta.permissions().readonly() => {
                    return Err(Error::Config(
                        "background care folder is not writable".into(),
                    ));
                }
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_owned()
}

fn describe(out: &Output) -> String {
    let code = out
        .status
        .code()
        .map_or_else(|| "signal".to_owned(), |c| c.to_string());
    format!("exit {code}: {}", stderr_text(out))
}

fn unique_backup(vault: &Path, path: &Path, bytes: &[u8]) -> Result<PathBuf, Error> {
    let backup_dir = crate::install::local_dir(vault)?;
    let hash = wi_lifecycle::sha256(path.as_os_str().as_encoded_bytes());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0u32..100 {
        let name = if attempt == 0 {
            format!("wi-backup-{stamp}")
        } else {
            format!("wi-backup-{stamp}-{attempt}")
        };
        let candidate = backup_dir.join(format!("{}-{name}", &hash[7..15]));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(mut file) => {
                file.write_all(bytes)?;
                wi_history::durable::barrier(&file, wi_history::durable::Durability::Standard)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Config(
        "could not allocate a unique backup name".into(),
    ))
}

fn plan_diff(path: &Path, old_text: &str, new_text: &str) -> String {
    let old: Vec<_> = old_text.lines().collect();
    let new: Vec<_> = new_text.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut diff = format!(
        "--- {}\n+++ {} (proposed)\n@@ -{},{} +{},{} @@\n",
        path.display(),
        path.display(),
        prefix + 1,
        old.len() - prefix - suffix,
        prefix + 1,
        new.len() - prefix - suffix
    );
    for line in &old[prefix..old.len() - suffix] {
        diff.push_str(&format!("-{line}\n"));
    }
    for line in &new[prefix..new.len() - suffix] {
        diff.push_str(&format!("+{line}\n"));
    }
    diff
}
