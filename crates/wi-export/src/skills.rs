//! Atomic, ownership-scoped procedure export for host skill discovery.
use crate::install::Error;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use wi_store::{ProcedureMode, Vault};

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillTarget {
    Claude,
    Agents,
}
impl SkillTarget {
    pub fn root(self, home: &Path) -> PathBuf {
        home.join(match self {
            Self::Claude => ".claude/skills",
            Self::Agents => ".agents/skills",
        })
    }
    pub fn for_host(host: crate::hosts::HostId) -> Option<Self> {
        use crate::hosts::HostId;
        match host {
            HostId::Claude => Some(Self::Claude),
            HostId::Codex | HostId::Agy | HostId::Gemini | HostId::Cursor | HostId::Opencode => {
                Some(Self::Agents)
            }
            HostId::ClaudeDesktop | HostId::Windsurf | HostId::Vscode | HostId::Zed => None,
        }
    }
}
#[derive(Debug, Default, Serialize)]
pub struct SkillSyncReport {
    pub created: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub skipped: Vec<PathBuf>,
    pub renamed: Vec<PathBuf>,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Owner {
    vault_id: String,
    skill_id: String,
}
struct Skill {
    owner: Owner,
    text: String,
}

fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_matches('-');
    out.into()
}
fn render(vault: &Vault) -> Result<BTreeMap<String, Skill>> {
    vault.catch_up().map_err(store_error)?;
    let mut candidates = Vec::new();
    let mut ids = BTreeMap::<String, usize>::new();
    for label in vault
        .procedures(ProcedureMode::Discovery)
        .map_err(store_error)?
    {
        let read = vault.read(&label.path).map_err(store_error)?;
        let note = wi_note::Note::parse(&read.bytes).map_err(|e| Error::Config(e.to_string()))?;
        let lifecycle = note.lifecycle();
        let fresh = vault
            .classify(&[(label.path, read.bytes.clone())], &Value::Null)
            .map_err(store_error)?;
        if lifecycle.lifecycle_type != wi_note::LifecycleType::Memory
            || lifecycle.kind != Some(wi_note::Kind::Procedure)
            || !fresh.first().is_some_and(|l| l.eligibility == "current")
            || lifecycle.status != Some(wi_note::Status::Active)
            || lifecycle.excluded.unwrap_or(false)
        {
            continue;
        }
        let Some(id) = lifecycle.skill_id.filter(|v| !v.is_empty()) else {
            continue;
        };
        *ids.entry(id.clone()).or_default() += 1;
        let front = note.frontmatter();
        let title = front
            .as_ref()
            .and_then(|f| f.get("title"))
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| id.clone());
        let hash = wi_lifecycle::sha256(id.as_bytes());
        let name = format!("wi-{}-{}", slug(&title), &hash[7..15]);
        let summary = lifecycle
            .summary
            .as_deref()
            .unwrap_or_else(|| note.body().trim());
        let description: String = summary
            .split_inclusive(['.', '!', '?', '\n'])
            .next()
            .unwrap_or(summary)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(200)
            .collect();
        let description = if description.is_empty() {
            format!("Procedure: {title}").chars().take(200).collect()
        } else {
            description
        };
        let text = format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}\n\n<!-- wiki-island skill_id={} checksum={} -->\n",
            serde_json::to_string(&name).map_err(|e| Error::Config(e.to_string()))?,
            serde_json::to_string(&description).map_err(|e| Error::Config(e.to_string()))?,
            note.body().trim_end(),
            serde_json::to_string(&id).map_err(|e| Error::Config(e.to_string()))?,
            read.checksum
        );
        candidates.push((
            name,
            Skill {
                owner: Owner {
                    vault_id: vault.vault_id().into(),
                    skill_id: id,
                },
                text,
            },
        ));
    }
    let mut rendered = BTreeMap::new();
    for (name, skill) in candidates {
        if ids[&skill.owner.skill_id] == 1 {
            rendered.insert(name, skill);
        }
    }
    Ok(rendered)
}
fn store_error(error: wi_store::StoreError) -> Error {
    Error::Config(error.to_string())
}
fn regular(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.is_file() && !meta.file_type().is_symlink()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn owner(path: &Path) -> Result<Option<Owner>> {
    let marker = path.join(".wi-owned");
    if !regular(&marker)? {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&std::fs::read(marker)?).ok())
}
fn safe_directory(path: &Path, home: &Path) -> Result<bool> {
    for ancestor in path.ancestors() {
        if ancestor == home {
            break;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => return Ok(false),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}
pub(crate) fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config("skill file has no directory".into()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    let durability = wi_history::durable::Durability::Standard;
    wi_history::durable::barrier(temp.as_file(), durability)?;
    temp.persist(path).map_err(|e| Error::Io(e.error))?;
    wi_history::durable::barrier_dir(parent, durability)?;
    Ok(())
}
fn remove_owned(path: &Path, report: &mut SkillSyncReport, apply: bool) -> Result<()> {
    // Never recursively delete a directory: preserve extra user files and links.
    let files = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    if files.iter().any(|f| {
        matches!(f.file_name().to_str(), Some("SKILL.md" | ".wi-owned"))
            && !f.file_type().is_ok_and(|t| t.is_file())
    }) {
        report.skipped.push(path.into());
        return Ok(());
    }
    let foreign = files
        .iter()
        .any(|f| !matches!(f.file_name().to_str(), Some("SKILL.md" | ".wi-owned")));
    report.removed += 1;
    if apply {
        let skill = path.join("SKILL.md");
        if regular(&skill)? {
            std::fs::remove_file(skill)?;
        }
        std::fs::remove_file(path.join(".wi-owned"))?;
        if !foreign {
            std::fs::remove_dir(path)?;
        }
    }
    Ok(())
}
fn reconcile(
    vault_id: &str,
    home: &Path,
    roots: &[PathBuf],
    desired: &BTreeMap<String, Skill>,
    apply: bool,
) -> Result<SkillSyncReport> {
    let mut report = SkillSyncReport::default();
    for root in roots.iter().collect::<BTreeSet<_>>() {
        if !safe_directory(root, home)? {
            report.skipped.push(root.clone());
            continue;
        }
        let mut resolved = BTreeMap::new();
        for (name, skill) in desired {
            let mut chosen = name.clone();
            // Keep an already assigned hashed name even if its title changes.
            if root.is_dir() {
                let mut existing = std::fs::read_dir(root)?.collect::<std::io::Result<Vec<_>>>()?;
                existing.sort_by_key(|entry| entry.file_name());
                for entry in existing {
                    let candidate = entry.file_name().to_string_lossy().into_owned();
                    let hashed = candidate.rsplit_once('-').is_some_and(|(_, suffix)| {
                        suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
                    });
                    if entry.file_type()?.is_dir()
                        && hashed
                        && owner(&entry.path())?.as_ref() == Some(&skill.owner)
                    {
                        chosen = candidate;
                        break;
                    }
                }
            }
            let mut attempt = 0;
            loop {
                let dir = root.join(&chosen);
                if !safe_directory(&dir, home)? {
                    report.skipped.push(dir);
                    break;
                }
                if !dir.exists() || owner(&dir)?.as_ref() == Some(&skill.owner) {
                    let legacy_foreign = name
                        .rsplit_once('-')
                        .map(|(legacy, _)| root.join(legacy))
                        .filter(|legacy| std::fs::symlink_metadata(legacy).is_ok())
                        .map(|legacy| {
                            if !safe_directory(&legacy, home)? {
                                return Ok(true);
                            }
                            owner(&legacy).map(|owned| owned.as_ref() != Some(&skill.owner))
                        })
                        .transpose()?
                        .unwrap_or(false);
                    if chosen != *name || legacy_foreign {
                        report.renamed.push(dir);
                    }
                    resolved.insert(chosen.clone(), skill);
                    break;
                }
                let hash = wi_lifecycle::sha256(
                    format!(
                        "{}:{}:{attempt}",
                        skill.owner.vault_id, skill.owner.skill_id
                    )
                    .as_bytes(),
                );
                chosen = format!("wi-{}-{}", slug(&skill.owner.skill_id), &hash[7..15]);
                attempt += 1;
                if attempt > 100 {
                    report.skipped.push(dir);
                    break;
                }
            }
        }
        for (name, skill) in &resolved {
            let dir = root.join(name);
            if !safe_directory(&dir, home)? {
                report.skipped.push(dir);
                continue;
            }
            let exists = dir.exists();
            if exists && owner(&dir)?.as_ref() != Some(&skill.owner) {
                report.skipped.push(dir);
                continue;
            }
            let file = dir.join("SKILL.md");
            if std::fs::symlink_metadata(&file).is_ok() && !regular(&file)? {
                report.skipped.push(dir);
                continue;
            }
            let text = skill.text.replacen(
                skill.text.lines().nth(1).unwrap_or(""),
                &format!(
                    "name: {}",
                    serde_json::to_string(name).map_err(|e| Error::Config(e.to_string()))?
                ),
                1,
            );
            if regular(&file)? && std::fs::read(&file)? == text.as_bytes() {
                report.unchanged += 1;
                continue;
            }
            if exists {
                report.updated += 1
            } else {
                report.created += 1
            }
            if apply {
                std::fs::create_dir_all(&dir)?;
                // Write ownership first so an interrupted export is recoverable on the next pass.
                atomic(
                    &dir.join(".wi-owned"),
                    &serde_json::to_vec(&skill.owner).map_err(|e| Error::Config(e.to_string()))?,
                )?;
                atomic(&file, text.as_bytes())?;
            }
        }
        if !root.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let dir = entry.path();
            if let Some(owned) = owner(&dir)? {
                let wanted = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| resolved.get(name))
                    .is_some_and(|skill| skill.owner == owned);
                if owned.vault_id == vault_id && !wanted {
                    remove_owned(&dir, &mut report, apply)?;
                }
            }
        }
    }
    Ok(report)
}
pub fn sync(
    vault: &Vault,
    home: &Path,
    hosts: &[SkillTarget],
    apply: bool,
) -> Result<SkillSyncReport> {
    let state = crate::local::Targets::load(vault.root())?;
    let home = state.as_ref().map_or(home, |state| state.home.as_path());
    let roots = state.as_ref().map_or_else(
        || {
            hosts
                .iter()
                .map(|target| target.root(home))
                .collect::<Vec<_>>()
        },
        |state| {
            state
                .skill_dirs
                .iter()
                .filter(|path| hosts.iter().any(|target| target.root(home) == **path))
                .cloned()
                .collect()
        },
    );
    let desired = render(vault)?;
    if apply && !desired.is_empty() {
        crate::install::record_created_dirs(vault.root(), &roots)?;
    }
    reconcile(vault.vault_id(), home, &roots, &desired, apply)
}
pub fn uninstall(vault: &Vault, home: &Path) -> Result<SkillSyncReport> {
    match crate::local::Targets::load(vault.root())? {
        Some(state) => uninstall_recorded(vault.vault_id(), &state),
        None => uninstall_by_id(vault.vault_id(), home),
    }
}
pub fn uninstall_by_id(vault_id: &str, home: &Path) -> Result<SkillSyncReport> {
    reconcile(
        vault_id,
        home,
        &[
            SkillTarget::Claude.root(home),
            SkillTarget::Agents.root(home),
        ],
        &BTreeMap::new(),
        true,
    )
}

pub fn uninstall_recorded(
    vault_id: &str,
    state: &crate::local::Targets,
) -> Result<SkillSyncReport> {
    reconcile(
        vault_id,
        &state.home,
        &state.skill_dirs,
        &BTreeMap::new(),
        true,
    )
}

/// Persist targets in the vault's editable config while retaining unknown TOML.
pub fn enable(vault: &Vault, hosts: &[SkillTarget]) -> Result<()> {
    let path = vault.root().join(".wiki-island/config.toml");
    let text = std::fs::read_to_string(&path)?;
    let mut config = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| Error::Config(e.to_string()))?;
    let mut targets: BTreeSet<_> = configured(vault).into_iter().collect();
    targets.extend(hosts);
    let values = targets
        .iter()
        .map(|host| match host {
            SkillTarget::Claude => "claude",
            SkillTarget::Agents => "agents",
        })
        .collect::<toml_edit::Array>();
    config["skill_targets"] = toml_edit::value(values);
    let after = config.to_string();
    wi_note::VaultConfig::parse(&after).map_err(|e| Error::Config(e.to_string()))?;
    if text != after {
        atomic(&path, after.as_bytes())?;
        let marker = digest_path(vault)?;
        if regular(&marker)? {
            std::fs::remove_file(marker)?;
        }
    }
    Ok(())
}
pub fn configured(vault: &Vault) -> Vec<SkillTarget> {
    // Read the config anew so setup's changes are visible through an already open Vault.
    std::fs::read_to_string(vault.root().join(".wiki-island/config.toml"))
        .ok()
        .and_then(|text| wi_note::VaultConfig::parse(&text).ok())
        .and_then(|config| {
            config
                .get("skill_targets")
                .and_then(|v| v.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|v| match v.as_str() {
                            Some("claude") => Some(SkillTarget::Claude),
                            Some("agents") => Some(SkillTarget::Agents),
                            _ => None,
                        })
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// Stop automatic exports while preserving the remaining vault configuration.
pub fn disable(vault: &Vault) -> Result<()> {
    disable_at(vault.root())
}
pub fn disable_at(root: &Path) -> Result<()> {
    let path = root.join(".wiki-island/config.toml");
    let before = std::fs::read_to_string(&path)?;
    let mut doc = before
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| Error::Config(e.to_string()))?;
    doc.remove("skill_targets");
    let after = doc.to_string();
    if before != after {
        atomic(&path, after.as_bytes())?;
    }
    Ok(())
}

/// Synchronize enabled targets only when the current procedures differ from
/// the last successfully exported set. Maintenance never consumes this marker.
pub fn sync_enabled(vault: &Vault, _home: &Path) -> Result<Option<SkillSyncReport>> {
    sync_recorded(vault)
}

/// Background synchronization has no environment-dependent destinations.
pub fn sync_recorded(vault: &Vault) -> Result<Option<SkillSyncReport>> {
    let Some(state) = crate::local::Targets::load(vault.root())? else {
        return Ok(None);
    };
    let targets = configured(vault);
    if targets.is_empty() {
        return Ok(None);
    }
    let marker = digest_path(vault)?;
    // The first export establishes a complete view even without a watcher.
    // Subsequent checks reuse the index maintained by the calling process.
    if !regular(&marker)? {
        vault.catch_up().map_err(store_error)?;
    }
    if vault.index_as_of().map_err(store_error)?.is_none() {
        vault.catch_up().map_err(store_error)?;
    }
    let digest = vault.procedures_digest().map_err(store_error)?;
    if regular(&marker)? && std::fs::read_to_string(&marker)? == digest {
        return Ok(None);
    }
    let report = sync(vault, &state.home, &targets, true)?;
    // A refused target must remain pending for the next automatic pass.
    if report.skipped.is_empty() {
        atomic(&marker, digest.as_bytes())?;
    }
    Ok(Some(report))
}

fn digest_path(vault: &Vault) -> Result<PathBuf> {
    let mut local = crate::install::local_dir(vault.root())?;
    local.pop();
    Ok(local.join("skills-digest"))
}
