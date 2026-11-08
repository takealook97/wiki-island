//! launchd LaunchAgent that keeps `wi watch` running for one vault (macOS only).
//!
//! Planning is read-only. Applying mirrors the host installer protections: a symlink or
//! non-regular plist is rejected, a stale preview fails, a differing file is backed up under a
//! unique name, and the replacement is atomic.
use crate::install::Error;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;

const LABEL_PREFIX: &str = "com.wiki-island.watch.";
const SERVICE_PATH_ENV: &str = "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin";
const LAUNCHCTL: &str = "/bin/launchctl";
/// `launchctl bootout` exit codes that mean "service is not loaded": 3 (ESRCH, "No such
/// process") and 113 ("Could not find specified service"). Exit 5 ("Input/output error") is
/// accepted only when stderr also says the service is missing; any other failure propagates.
const BOOTOUT_NOT_LOADED_CODES: [i32; 2] = [3, 113];
const BOOTOUT_IO_ERROR_CODE: i32 = 5;

/// Runs `launchctl` with the given arguments. Tests substitute a fake.
pub trait Launchctl {
    fn run(&self, args: &[&str]) -> std::io::Result<Output>;
}

/// Executes `WI_LAUNCHCTL`, or the system `/bin/launchctl` when unset.
pub struct SystemLaunchctl;

impl Launchctl for SystemLaunchctl {
    fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        std::process::Command::new(
            std::env::var_os("WI_LAUNCHCTL").unwrap_or_else(|| LAUNCHCTL.into()),
        )
        .args(args)
        .output()
    }
}

#[derive(Debug)]
pub struct ServicePlan {
    pub label: String,
    pub plist_path: PathBuf,
    pub log_path: PathBuf,
    pub contents: String,
    pub existing: Option<String>,
    vault: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub label: String,
    pub installed: bool,
    pub loaded: bool,
    pub plist_path: PathBuf,
    pub log_path: PathBuf,
}

struct Paths {
    label: String,
    plist_path: PathBuf,
    log_path: PathBuf,
}

fn ensure_supported() -> Result<(), Error> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        Err(Error::Unsupported(
            "wi service requires macOS launchd; run wi watch to keep this vault up to date".into(),
        ))
    }
}

fn paths(home: &Path, vault_id: &str) -> Result<Paths, Error> {
    ensure_supported()?;
    if vault_id.is_empty()
        || !vault_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Error::Config(format!(
            "invalid vault id {vault_id:?}; use only A-Z, a-z, 0-9, '_' and '-'"
        )));
    }
    if !home.is_absolute() {
        return Err(Error::Config("home must be an absolute path".into()));
    }
    let label = format!("{LABEL_PREFIX}{vault_id}");
    Ok(Paths {
        plist_path: home
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist")),
        log_path: home
            .join("Library/Logs/wiki-island")
            .join(format!("{vault_id}.log")),
        label,
    })
}

fn read_file(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.is_file() || m.file_type().is_symlink() => Err(Error::Config(
            "service plist must be a regular file, not a symlink".into(),
        )),
        Ok(_) => String::from_utf8(std::fs::read(path)?)
            .map(Some)
            .map_err(|e| Error::Config(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
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
            c => out.push(c),
        }
    }
    out
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

fn render(label: &str, binary: &str, vault: &str, log: &str, home: &str) -> String {
    let (label, binary, vault, log) = (
        xml_escape(label),
        xml_escape(binary),
        xml_escape(vault),
        xml_escape(log),
    );
    let home = xml_escape(home);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{binary}</string>
		<string>watch</string>
		<string>--vault</string>
		<string>{vault}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Background</string>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>HOME</key>
		<string>{home}</string>
		<key>PATH</key>
		<string>{SERVICE_PATH_ENV}</string>
	</dict>
</dict>
</plist>
"#
    )
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
    let p = paths(home, vault_id)?;
    crate::install::preflight_local(vault_root)?;
    let binary = absolute_str(wi_binary, "wi binary")?;
    if !allow_dev_binary && is_dev_binary(wi_binary) {
        return Err(Error::Config("service needs a stable binary path; run `cargo install --path crates/wi-cli` or explicitly pass --allow-dev-binary".into()));
    }
    let vault = absolute_str(vault_root, "vault root")?;
    let log = absolute_str(&p.log_path, "log path")?;
    let existing = read_file(&p.plist_path)?;
    for directory in [p.plist_path.parent(), p.log_path.parent()]
        .into_iter()
        .flatten()
    {
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
    let contents = render(&p.label, binary, vault, log, absolute_str(home, "home")?);
    Ok(ServicePlan {
        label: p.label,
        plist_path: p.plist_path,
        log_path: p.log_path,
        contents,
        existing,
        vault: vault_root.to_path_buf(),
    })
}

#[cfg(unix)]
fn user_id(home: &Path) -> Result<u32, Error> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(home)?.uid())
}

#[cfg(not(unix))]
fn user_id(_home: &Path) -> Result<u32, Error> {
    Err(Error::Unsupported(
        "wi service requires macOS launchd; run wi watch to keep this vault up to date".into(),
    ))
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

fn is_not_loaded(out: &Output) -> bool {
    match out.status.code() {
        Some(code) if BOOTOUT_NOT_LOADED_CODES.contains(&code) => true,
        Some(BOOTOUT_IO_ERROR_CODE) => {
            let err = stderr_text(out).to_ascii_lowercase();
            err.contains("no such process") || err.contains("could not find")
        }
        _ => false,
    }
}

/// Unloads the service. A recognized not-loaded result is not an error; returns whether the
/// service had been loaded.
fn bootout(launchctl: &dyn Launchctl, uid: u32, label: &str) -> Result<bool, Error> {
    let target = format!("gui/{uid}/{label}");
    let out = launchctl.run(&["bootout", &target])?;
    if out.status.success() {
        Ok(true)
    } else if is_not_loaded(&out) {
        Ok(false)
    } else {
        Err(Error::Service(format!(
            "launchctl bootout {target} failed ({})",
            describe(&out)
        )))
    }
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

impl ServicePlan {
    /// Line diff between the existing plist and the proposed one; empty when identical.
    pub fn diff(&self) -> String {
        if self.is_noop() {
            return String::new();
        }
        plan_diff(
            &self.plist_path,
            self.existing.as_deref().unwrap_or(""),
            &self.contents,
        )
    }

    /// True when the plist bytes already match. Says nothing about whether it is loaded.
    pub fn is_noop(&self) -> bool {
        self.existing.as_deref() == Some(self.contents.as_str())
    }

    /// Caller has already printed the diff and explicitly selected --yes. Always (re)loads the
    /// service, even when the file is unchanged.
    pub fn apply(&self, launchctl: &dyn Launchctl) -> Result<(), Error> {
        let parent = self
            .plist_path
            .parent()
            .ok_or_else(|| Error::Config("plist has no parent".into()))?;
        let log_dir = self
            .log_path
            .parent()
            .ok_or_else(|| Error::Config("log path has no parent".into()))?;
        let home = parent
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| Error::Config("plist path is not under a home directory".into()))?;
        let durability = wi_history::durable::Durability::Standard;
        crate::install::record_created_dirs(
            &self.vault,
            &[parent.to_path_buf(), log_dir.to_path_buf()],
        )?;
        wi_history::durable::create_dir_durable(parent, durability)?;
        wi_history::durable::create_dir_durable(log_dir, durability)?;
        // Validate the live destination before any launchctl call, noop or not.
        if read_file(&self.plist_path)? != self.existing {
            return Err(Error::Stale);
        }
        if !self.is_noop() {
            if let Some(old) = &self.existing {
                unique_backup(&self.vault, &self.plist_path, old.as_bytes())?;
                wi_history::durable::barrier_dir(parent, durability)?;
            }
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            temp.write_all(self.contents.as_bytes())?;
            wi_history::durable::barrier(temp.as_file(), durability)?;
            if read_file(&self.plist_path)? != self.existing {
                return Err(Error::Stale);
            }
            temp.persist(&self.plist_path)
                .map_err(|e| Error::Io(e.error))?;
            wi_history::durable::barrier_dir(parent, durability)?;
        }
        let uid = user_id(home)?;
        if let Err(error) = bootout(launchctl, uid, &self.label) {
            self.restore()?;
            return Err(error);
        }
        let domain = format!("gui/{uid}");
        let plist = self
            .plist_path
            .to_str()
            .ok_or_else(|| Error::Config("plist path must be valid UTF-8".into()))?;
        let attempt = launchctl.run(&["bootstrap", &domain, plist]);
        let failure = match attempt {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => Error::Service(format!(
                "launchctl bootstrap {domain} failed ({})",
                describe(&out)
            )),
            Err(error) => Error::Io(error),
        };
        if let Err(error) = self.restore() {
            return Err(Error::Service(format!(
                "{failure}; restoring previous plist failed: {error}"
            )));
        }
        if self.existing.is_some() {
            match launchctl.run(&["bootstrap", &domain, plist]) {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    return Err(Error::Service(format!(
                        "{failure}; previous service bootstrap failed ({})",
                        describe(&out)
                    )));
                }
                Err(error) => {
                    return Err(Error::Service(format!(
                        "{failure}; previous service bootstrap failed: {error}"
                    )));
                }
            }
        }
        Err(failure)
    }

    /// Restore the previewed bytes atomically; never overwrite a concurrent edit.
    fn restore(&self) -> Result<(), Error> {
        if read_file(&self.plist_path)?.as_deref() != Some(self.contents.as_str()) {
            return Err(Error::Stale);
        }
        let parent = self
            .plist_path
            .parent()
            .ok_or_else(|| Error::Config("plist has no parent".into()))?;
        let durability = wi_history::durable::Durability::Standard;
        if let Some(old) = &self.existing {
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            temp.write_all(old.as_bytes())?;
            wi_history::durable::barrier(temp.as_file(), durability)?;
            temp.persist(&self.plist_path)
                .map_err(|e| Error::Io(e.error))?;
        } else {
            std::fs::remove_file(&self.plist_path)?;
        }
        wi_history::durable::barrier_dir(parent, durability)?;
        Ok(())
    }
}

/// Unloads the service and removes its plist. Returns whether a plist was removed.
pub fn uninstall(home: &Path, vault_id: &str, launchctl: &dyn Launchctl) -> Result<bool, Error> {
    let p = paths(home, vault_id)?;
    let present = read_file(&p.plist_path)?.is_some();
    // Always unload: a loaded job can outlive a deleted plist.
    bootout(launchctl, user_id(home)?, &p.label)?;
    for path in [&p.log_path, &p.log_path.with_extension("log.old")] {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
                std::fs::remove_file(path)?;
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if present {
        std::fs::remove_file(&p.plist_path)?;
    }
    Ok(present)
}

pub fn status(
    home: &Path,
    vault_id: &str,
    launchctl: &dyn Launchctl,
) -> Result<ServiceStatus, Error> {
    let p = paths(home, vault_id)?;
    let installed = read_file(&p.plist_path)?.is_some();
    let target = format!("gui/{}/{}", user_id(home)?, p.label);
    let loaded = launchctl.run(&["print", &target])?.status.success();
    Ok(ServiceStatus {
        label: p.label,
        installed,
        loaded,
        plist_path: p.plist_path,
        log_path: p.log_path,
    })
}

/// Keep a single previous service log while preserving launchd's open file
/// descriptor: copy, then truncate the same file rather than renaming it.
pub fn rotate_log(home: &Path, vault_id: &str) -> Result<(), Error> {
    const LOG_MAX_BYTES: u64 = 1024 * 1024;
    let path = paths(home, vault_id)?.log_path;
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() < LOG_MAX_BYTES {
        return Ok(());
    }
    let previous = path.with_extension("log.old");
    if std::fs::symlink_metadata(&previous)
        .is_ok_and(|m| !m.is_file() || m.file_type().is_symlink())
    {
        return Err(Error::Config(
            "Previous service log must be a regular file.".into(),
        ));
    }
    // Copy at most the cap even if another writer appended a large error.
    let source = std::fs::File::open(&path)?;
    let mut target = std::fs::File::create(&previous)?;
    std::io::copy(&mut std::io::Read::take(source, LOG_MAX_BYTES), &mut target)?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_len(0)?;
    Ok(())
}
