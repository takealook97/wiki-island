//! Diff-first host registration. Planning is read-only; applying requires --yes.
use crate::Host;
use crate::hosts::{Format, HostId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid host config: {0}")]
    Config(String),
    #[error("host config changed since preview; retry the command")]
    Stale,
    #[error(transparent)]
    History(#[from] wi_history::HistoryError),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("service error: {0}")]
    Service(String),
}
#[derive(Debug, Serialize)]
pub struct Plan {
    pub path: PathBuf,
    pub diff: String,
    pub changed: bool,
    pub warnings: Vec<String>,
    #[serde(skip)]
    permissions: Option<std::fs::Permissions>,
    #[serde(skip)]
    before: Option<Vec<u8>>,
    #[serde(skip)]
    after: Vec<u8>,
    #[serde(skip)]
    vault: PathBuf,
    #[serde(skip)]
    removing: bool,
    #[serde(skip)]
    remove_file: bool,
    #[serde(skip)]
    original: Option<Vec<u8>>,
}
fn read_file(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.is_file() || m.file_type().is_symlink() => Err(Error::Config(
            "config must be a regular file, not a symlink".into(),
        )),
        Ok(_) => Ok(Some(std::fs::read(path)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn plan(
    host: Host,
    home: &Path,
    codex_home: Option<&Path>,
    binary: &Path,
    vault: &Path,
) -> Result<Plan, Error> {
    plan_host(host.into(), home, codex_home, binary, vault)
}

pub fn plan_host(
    host: HostId,
    home: &Path,
    codex_home: Option<&Path>,
    binary: &Path,
    vault: &Path,
) -> Result<Plan, Error> {
    let path = host.config_path(home, codex_home);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    // Resolve the link once so atomic replacement updates its target, not the link.
    let path = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(&path)?,
        _ => path,
    };
    let before = read_file(&path)?;
    let permissions = before
        .as_ref()
        .map(|_| std::fs::metadata(&path).map(|m| m.permissions()))
        .transpose()?;
    let mut warnings = Vec::new();
    let text = before
        .as_deref()
        .map(std::str::from_utf8)
        .transpose()
        .map_err(|e| Error::Config(e.to_string()))?
        .unwrap_or("");
    let args = vec![
        "mcp".to_owned(),
        "--vault".to_owned(),
        vault.to_string_lossy().into_owned(),
    ];
    let command = binary.to_string_lossy().into_owned();
    let after = if host == HostId::Codex {
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| Error::Config(e.to_string()))?;
        if doc.get("mcp_servers").is_some_and(|i| !i.is_table_like()) {
            return Err(Error::Config("mcp_servers must be a table".into()));
        }
        if doc
            .get("mcp_servers")
            .and_then(|i| i.get("wiki-island"))
            .is_some_and(|i| !i.is_table_like())
        {
            return Err(Error::Config("wiki-island must be a table".into()));
        }
        if let Some(table) = doc
            .get_mut("mcp_servers")
            .and_then(|i| i.get_mut("wiki-island"))
            .and_then(|i| i.as_table_like_mut())
        {
            for key in [
                "url",
                "http_headers",
                "env_http_headers",
                "bearer_token_env_var",
            ] {
                table.remove(key);
            }
        }
        // Standard tables remain extensible when another server is appended later.
        for keys in [&["mcp_servers"][..], &["mcp_servers", "wiki-island"][..]] {
            let item = if keys.len() == 1 {
                &mut doc[keys[0]]
            } else {
                &mut doc[keys[0]][keys[1]]
            };
            if item.is_none() {
                *item = toml_edit::Item::Table(toml_edit::Table::new());
            } else if let Some(inline) = item.as_inline_table() {
                *item = toml_edit::Item::Table(inline.clone().into_table());
            }
        }
        if let Some(table) = doc["mcp_servers"].as_table_mut() {
            table.set_implicit(true);
        }
        doc["mcp_servers"]["wiki-island"]["command"] = toml_edit::value(command);
        doc["mcp_servers"]["wiki-island"]["args"] = toml_edit::value(
            args.iter()
                .map(|v| toml_edit::Value::from(v.as_str()))
                .collect::<toml_edit::Array>(),
        );
        doc.to_string().into_bytes()
    } else {
        let Format::Json(root_key) = host.spec().format else {
            unreachable!()
        };
        let mut doc: Value = if text.is_empty() {
            json!({})
        } else {
            parse_json_config(text)?
        };
        let obj = doc
            .as_object_mut()
            .ok_or_else(|| Error::Config("root must be an object".into()))?;
        let servers = obj
            .entry(root_key)
            .or_insert(json!({}))
            .as_object_mut()
            .ok_or_else(|| Error::Config(format!("{root_key} must be an object")))?;
        let entry = servers
            .entry("wiki-island")
            .or_insert(json!({}))
            .as_object_mut()
            .ok_or_else(|| Error::Config("wiki-island must be an object".into()))?;
        for key in ["url", "httpUrl", "headers"] {
            entry.remove(key);
        }
        if matches!(host, HostId::Claude | HostId::Vscode) {
            entry.insert("type".into(), json!("stdio"));
        }
        if host == HostId::Agy {
            entry.insert("disabled".into(), json!(false));
        }
        entry.insert("command".into(), json!(&command));
        entry.insert("args".into(), json!(&args));
        if host == HostId::Zed {
            entry.insert("command".into(), json!({"path": command, "args": args}));
            entry.remove("args");
        }
        if host == HostId::Opencode {
            entry.insert("type".into(), json!("local"));
            entry.insert("enabled".into(), json!(true));
            entry.insert(
                "command".into(),
                json!(std::iter::once(command).chain(args).collect::<Vec<_>>()),
            );
            entry.remove("args");
        }
        let replacement = doc[root_key]["wiki-island"].clone();
        if text.is_empty() {
            let mut encoded =
                serde_json::to_vec_pretty(&doc).map_err(|e| Error::Config(e.to_string()))?;
            encoded.push(b'\n');
            encoded
        } else if let Some(edited) = replace_entry(text, &[root_key, "wiki-island"], &replacement) {
            edited.into_bytes()
        } else {
            let warning =
                "JSON span scan failed; host config formatting will be regenerated".to_owned();
            warnings.push(warning);
            let mut encoded =
                serde_json::to_vec_pretty(&doc).map_err(|e| Error::Config(e.to_string()))?;
            encoded.push(b'\n');
            encoded
        }
    };
    let original = match load_config(vault, &path)? {
        Some(saved) if before.as_deref() == Some(saved.installed.as_slice()) => saved.original,
        Some(saved) => {
            match before.as_deref().and_then(|current| {
                undo_installation(
                    saved.original.as_deref().unwrap_or_default(),
                    &saved.installed,
                    current,
                )
            }) {
                Some(restored) => Some(restored),
                None => plan_remove(host, home, codex_home, vault)?
                    .map(|plan| plan.after)
                    .or_else(|| before.clone()),
            }
        }
        None => before.clone(),
    };
    let changed = before.as_deref() != Some(after.as_slice());
    // A line diff with common lines suppressed. No shell invocation or execution.
    let old: Vec<_> = text.lines().collect();
    let new_text = String::from_utf8_lossy(&after);
    let new: Vec<_> = new_text.lines().collect();
    // An idempotent plan has no diff at all, not an empty header.
    let mut diff = String::new();
    if changed {
        diff.push_str(&format!(
            "--- {}\n+++ {} (proposed)\n",
            path.display(),
            path.display()
        ));
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        diff.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            prefix + 1,
            old.len() - prefix - suffix,
            prefix + 1,
            new.len() - prefix - suffix
        ));
        for line in &old[prefix..old.len() - suffix] {
            diff.push_str(&format!("-{line}\n"));
        }
        for line in &new[prefix..new.len() - suffix] {
            diff.push_str(&format!("+{line}\n"));
        }
    }
    Ok(Plan {
        path,
        diff,
        changed,
        warnings,
        permissions,
        before,
        after,
        vault: vault.to_path_buf(),
        removing: false,
        remove_file: false,
        original,
    })
}
impl Plan {
    /// Validate destination containers without creating them.
    pub fn preflight(&self) -> Result<(), Error> {
        preflight_local(&self.vault)?;
        if read_file(&self.path)? != self.before {
            return Err(Error::Stale);
        }
        for parent in self.path.parent().into_iter().flat_map(Path::ancestors) {
            match std::fs::metadata(parent) {
                Ok(meta) if !meta.is_dir() || meta.permissions().readonly() => {
                    return Err(Error::Config("settings folder is not writable".into()));
                }
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        if self
            .permissions
            .as_ref()
            .is_some_and(std::fs::Permissions::readonly)
        {
            return Err(Error::Config("settings file is not writable".into()));
        }
        Ok(())
    }

    /// Caller has already printed the diff and explicitly selected --yes.
    pub fn apply(&self) -> Result<Option<PathBuf>, Error> {
        if !self.changed {
            if self.removing {
                forget_config(&self.vault, &self.path)?;
            }
            return Ok(None);
        }
        if read_file(&self.path)? != self.before {
            return Err(Error::Stale);
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::Config("config has no parent".into()))?;
        if !self.removing {
            record_created_dirs(&self.vault, &[parent.to_path_buf()])?;
        }
        wi_history::durable::create_dir_durable(parent, wi_history::durable::Durability::Standard)?;
        let backup = if self.removing {
            None
        } else {
            save_config(
                &self.vault,
                &self.path,
                self.original.clone(),
                self.after.clone(),
            )?
        };
        if self.remove_file {
            std::fs::remove_file(&self.path)?;
            forget_config(&self.vault, &self.path)?;
            wi_history::durable::barrier_dir(parent, wi_history::durable::Durability::Standard)?;
            return Ok(None);
        }
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(&self.after)?;
        if let Some(permissions) = &self.permissions {
            temp.as_file().set_permissions(permissions.clone())?;
        }
        wi_history::durable::barrier(temp.as_file(), wi_history::durable::Durability::Standard)?;
        if read_file(&self.path)? != self.before {
            return Err(Error::Stale);
        }
        temp.persist(&self.path).map_err(|e| Error::Io(e.error))?;
        wi_history::durable::barrier_dir(parent, wi_history::durable::Durability::Standard)?;
        if self.removing {
            forget_config(&self.vault, &self.path)?;
        }
        Ok(backup)
    }
}

/// A validated JSON value's end; string escapes and nested containers are skipped.
fn value_end(bytes: &[u8], start: usize) -> Option<usize> {
    let first = *bytes.get(start)?;
    if first == b'"' {
        let mut i = start + 1;
        while let Some(&byte) = bytes.get(i) {
            match byte {
                b'\\' => i += 2,
                b'"' => return Some(i + 1),
                _ => i += 1,
            }
        }
        return None;
    }
    if first == b'{' || first == b'[' {
        let closing = if first == b'{' { b'}' } else { b']' };
        let mut i = start + 1;
        while let Some(&byte) = bytes.get(i) {
            if byte == closing {
                return Some(i + 1);
            }
            if matches!(byte, b'"' | b'{' | b'[') {
                i = value_end(bytes, i)?;
            } else {
                i += 1;
            }
        }
        return None;
    }
    let mut i = start;
    while bytes
        .get(i)
        .is_some_and(|c| !c.is_ascii_whitespace() && !matches!(c, b',' | b'}' | b']'))
    {
        i += 1;
    }
    (i > start).then_some(i)
}

fn whitespace(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// Replace only the requested value, or insert only the missing member. JSON is
/// validated by serde before scanning; duplicate keys trigger the explicit fallback.
fn replace_entry(text: &str, keys: &[&str], replacement: &Value) -> Option<String> {
    let scan = json_scan(text)?;
    let bytes = scan.as_bytes();
    let start = whitespace(bytes, 0);
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut i = whitespace(bytes, start + 1);
    let mut found = None;
    let mut names = std::collections::HashSet::new();
    while bytes.get(i) != Some(&b'}') {
        if bytes.get(i) != Some(&b'"') {
            return None;
        }
        let end = value_end(bytes, i)?;
        let name: String = serde_json::from_str(&text[i..end]).ok()?;
        if !names.insert(name.clone()) {
            return None;
        }
        i = whitespace(bytes, end);
        if bytes.get(i) != Some(&b':') {
            return None;
        }
        i = whitespace(bytes, i + 1);
        let end = value_end(bytes, i)?;
        if name == keys[0] {
            found = Some(i..end);
        }
        i = whitespace(bytes, end);
        if bytes.get(i) == Some(&b',') {
            i = whitespace(bytes, i + 1);
        } else if bytes.get(i) != Some(&b'}') {
            return None;
        }
    }
    if let Some(span) = found {
        let before = &text[span.clone()];
        let after = if keys.len() > 1 {
            replace_entry(before, &keys[1..], replacement)?
        } else if parse_json_config(before).ok().as_ref() == Some(replacement) {
            return Some(text.to_owned());
        } else {
            serde_json::to_string(replacement).ok()?
        };
        let mut result = text.to_owned();
        result.replace_range(span, &after);
        Some(result)
    } else {
        let mut value = replacement.clone();
        for key in keys[1..].iter().rev() {
            value = json!({*key: value});
        }
        let member = format!(
            "{}{}:{}",
            if names.is_empty()
                || bytes[..i].iter().rev().find(|b| !b.is_ascii_whitespace()) == Some(&b',')
            {
                ""
            } else {
                ","
            },
            serde_json::to_string(keys[0]).ok()?,
            serde_json::to_string(&value).ok()?
        );
        let mut result = text.to_owned();
        result.insert_str(i, &member);
        Some(result)
    }
}

/// Remove this vault's registration only. A registration repointed elsewhere is retained.
pub fn plan_remove(
    host: HostId,
    home: &Path,
    codex_home: Option<&Path>,
    vault: &Path,
) -> Result<Option<Plan>, Error> {
    let path = host.config_path(home, codex_home);
    let path = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(&path)?,
        _ => std::path::absolute(path)?,
    };
    plan_remove_at(host, &path, vault)
}

/// Remove a registration only from the absolute destination recorded at setup.
pub fn plan_remove_at(host: HostId, path: &Path, vault: &Path) -> Result<Option<Plan>, Error> {
    if !path.is_absolute() {
        return Err(Error::Config("recorded host path must be absolute".into()));
    }
    let path = path.to_path_buf();
    let Some(before) = read_file(&path)? else {
        return Ok(None);
    };
    let permissions = Some(std::fs::metadata(&path)?.permissions());
    let text = std::str::from_utf8(&before).map_err(|e| Error::Config(e.to_string()))?;
    let args = json!(["mcp", "--vault", vault.to_string_lossy()]);
    let after = match host.spec().format {
        Format::Toml => {
            let mut doc = text
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| Error::Config(e.to_string()))?;
            let entry = doc.get("mcp_servers").and_then(|v| v.get("wiki-island"));
            let owned = entry
                .and_then(|v| v.get("args"))
                .and_then(|v| v.as_array())
                .map(|values| json!(values.iter().map(|v| v.as_str()).collect::<Vec<_>>()))
                == Some(args);
            if !owned {
                return Ok(None);
            }
            if let Some(table) = doc
                .get_mut("mcp_servers")
                .and_then(|v| v.as_table_like_mut())
            {
                table.remove("wiki-island");
            }
            doc.to_string().into_bytes()
        }
        Format::Json(key) => {
            let doc: Value = parse_json_config(text)?;
            let entry = &doc[key]["wiki-island"];
            let entry_args = match host {
                HostId::Zed => entry.pointer("/command/args").cloned(),
                HostId::Opencode => entry["command"]
                    .as_array()
                    .map(|v| json!(v.iter().skip(1).collect::<Vec<_>>())),
                _ => entry.get("args").cloned(),
            };
            if entry_args != Some(args) {
                return Ok(None);
            }
            remove_entry(text, &[key, "wiki-island"])
                .ok_or_else(|| {
                    Error::Config(
                        "cannot safely remove the registration from this JSON file".into(),
                    )
                })?
                .into_bytes()
        }
    };
    let saved = load_config(vault, &path)?;
    let (after, remove_file) = match saved {
        Some(saved) if saved.installed == before => {
            let missing = saved.original.is_none();
            (saved.original.unwrap_or_default(), missing)
        }
        _ => (after, false),
    };
    Ok(Some(Plan {
        path,
        diff: "Remove this vault's Wiki Island connection.\n".into(),
        changed: before != after,
        warnings: vec![],
        permissions,
        before: Some(before),
        after,
        vault: vault.to_path_buf(),
        removing: true,
        remove_file,
        original: None,
    }))
}

/// Delete exactly one JSON member, retaining all unrelated values and formatting.
fn remove_entry(text: &str, keys: &[&str]) -> Option<String> {
    let scan = json_scan(text)?;
    let bytes = scan.as_bytes();
    let start = whitespace(bytes, 0);
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut i = whitespace(bytes, start + 1);
    let mut previous_comma = None;
    let mut found = None;
    let mut names = std::collections::HashSet::new();
    while bytes.get(i) != Some(&b'}') {
        let member_start = i;
        let key_end = value_end(bytes, i)?;
        let name: String = serde_json::from_str(&text[i..key_end]).ok()?;
        if !names.insert(name.clone()) {
            return None;
        }
        i = whitespace(bytes, key_end);
        if bytes.get(i) != Some(&b':') {
            return None;
        }
        let value_start = whitespace(bytes, i + 1);
        let end = value_end(bytes, value_start)?;
        i = whitespace(bytes, end);
        let comma = (bytes.get(i) == Some(&b',')).then_some(i);
        if name == keys[0] {
            found = Some((member_start, value_start, end, comma, previous_comma));
        }
        if let Some(comma) = comma {
            previous_comma = Some(comma);
            i = whitespace(bytes, comma + 1);
        } else if bytes.get(i) != Some(&b'}') {
            return None;
        }
    }
    let (member_start, value_start, end, comma, previous_comma) = found?;
    let mut result = text.to_owned();
    if keys.len() > 1 {
        let child = remove_entry(&text[value_start..end], &keys[1..])?;
        result.replace_range(value_start..end, &child);
    } else {
        if let Some(comma) = comma {
            result.replace_range(member_start..comma + 1, "");
        } else {
            result.replace_range(member_start..end, "");
            if let Some(comma) = previous_comma {
                result.replace_range(comma..comma + 1, "");
            }
        }
    }
    Some(result)
}

/// Mask JSONC comments without changing byte offsets or quoted strings.
fn json_scan(text: &str) -> Option<String> {
    let mut bytes = text.as_bytes().to_vec();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i = value_end(&bytes, i)?;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    bytes[i] = b' ';
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                bytes[i] = b' ';
                bytes[i + 1] = b' ';
                i += 2;
                loop {
                    if i + 1 >= bytes.len() {
                        return None;
                    }
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        bytes[i] = b' ';
                        bytes[i + 1] = b' ';
                        i += 2;
                        break;
                    }
                    if bytes[i] != b'\n' && bytes[i] != b'\r' {
                        bytes[i] = b' ';
                    }
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8(bytes).ok()
}
fn parse_json_config(text: &str) -> Result<Value, Error> {
    let scan = json_scan(text)
        .ok_or_else(|| Error::Config("unfinished comment or string in host settings".into()))?;
    let mut bytes = scan.into_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            i = value_end(&bytes, i)
                .ok_or_else(|| Error::Config("unfinished string in host settings".into()))?;
        } else {
            if bytes[i] == b',' && matches!(bytes.get(whitespace(&bytes, i + 1)), Some(b'}' | b']'))
            {
                bytes[i] = b' ';
            }
            i += 1;
        }
    }
    serde_json::from_slice(&bytes).map_err(|e| Error::Config(e.to_string()))
}

/// Undo an unchanged installation splice while retaining edits outside it.
fn undo_installation(original: &[u8], installed: &[u8], current: &[u8]) -> Option<Vec<u8>> {
    let prefix = original
        .iter()
        .zip(installed)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = original[prefix..]
        .iter()
        .rev()
        .zip(installed[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let inserted = &installed[prefix..installed.len() - suffix];
    if inserted.is_empty() {
        return None;
    }
    let mut positions = current
        .windows(inserted.len())
        .enumerate()
        .filter_map(|(index, bytes)| (bytes == inserted).then_some(index));
    let index = positions.next()?;
    if positions.next().is_some() {
        return None;
    }
    let mut restored = current.to_vec();
    restored.splice(
        index..index + inserted.len(),
        original[prefix..original.len() - suffix].iter().copied(),
    );
    Some(restored)
}

#[derive(Serialize, Deserialize)]
struct SavedConfig {
    original: Option<Vec<u8>>,
    installed: Vec<u8>,
}

/// The per-device local directory of `vault` (S2) and the containers from
/// the first one this crate checks: git mode checks the vault root,
/// `.wiki-island` and `.wiki-island/local`; folder mode checks only
/// `<data>/vaults/<vault_id>/local`, which the engine owns.
pub(crate) fn local_chain(vault: &Path) -> Vec<PathBuf> {
    let base = wi_history::local_state_dir(vault, None)
        .unwrap_or_else(|_| vault.join(wi_history::VAULT_LOCAL_DIR));
    if base == vault.join(wi_history::VAULT_LOCAL_DIR) {
        vec![
            vault.to_path_buf(),
            vault.join(".wiki-island"),
            base.clone(),
            base.join("backups"),
        ]
    } else {
        vec![base.clone(), base.join("backups")]
    }
}

/// The per-device local directory of `vault` (`.wiki-island/local` in git
/// mode, the data directory in folder mode).
pub(crate) fn local_base(vault: &Path) -> PathBuf {
    wi_history::local_state_dir(vault, None)
        .unwrap_or_else(|_| vault.join(wi_history::VAULT_LOCAL_DIR))
}

/// Check local ownership and backup containers without creating anything.
pub fn preflight_local(vault: &Path) -> Result<(), Error> {
    for path in local_chain(vault) {
        match std::fs::symlink_metadata(&path) {
            Ok(meta)
                if !meta.is_dir()
                    || meta.file_type().is_symlink()
                    || meta.permissions().readonly() =>
            {
                return Err(Error::Config(
                    "local settings folder must be a writable regular directory".into(),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub(crate) fn local_dir(vault: &Path) -> Result<PathBuf, Error> {
    let chain = local_chain(vault);
    let path = chain.last().cloned().unwrap_or_default();
    // The vault root itself (git mode) already exists.
    let skip = usize::from(chain.len() == 4);
    for dir in chain.iter().skip(skip) {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
                return Err(Error::Config(
                    "local backup folder must be a regular directory".into(),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                wi_history::durable::create_dir_durable(
                    dir,
                    wi_history::durable::Durability::Standard,
                )?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}
fn record_path(vault: &Path, target: &Path, extension: &str) -> PathBuf {
    let hash = wi_lifecycle::sha256(target.as_os_str().as_encoded_bytes());
    local_base(vault)
        .join("backups")
        .join(format!("{}.{}", &hash[7..], extension))
}
fn private_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config("backup has no folder".into()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    let durability = wi_history::durable::Durability::Standard;
    wi_history::durable::barrier(temp.as_file(), durability)?;
    temp.persist(path).map_err(|e| Error::Io(e.error))?;
    wi_history::durable::barrier_dir(parent, durability)?;
    Ok(())
}
fn load_config(vault: &Path, target: &Path) -> Result<Option<SavedConfig>, Error> {
    read_file(&record_path(vault, target, "json"))?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|e| Error::Config(e.to_string())))
        .transpose()
}
fn save_config(
    vault: &Path,
    target: &Path,
    original: Option<Vec<u8>>,
    installed: Vec<u8>,
) -> Result<Option<PathBuf>, Error> {
    local_dir(vault)?;
    let saved = SavedConfig {
        original,
        installed,
    };
    let backup = if let Some(bytes) = &saved.original {
        let path = record_path(vault, target, "original");
        private_atomic(&path, bytes)?;
        Some(path)
    } else {
        None
    };
    private_atomic(
        &record_path(vault, target, "json"),
        &serde_json::to_vec(&saved).map_err(|e| Error::Config(e.to_string()))?,
    )?;
    Ok(backup)
}
fn forget_config(vault: &Path, target: &Path) -> Result<(), Error> {
    for extension in ["json", "original"] {
        let path = record_path(vault, target, extension);
        if read_file(&path)?.is_some() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Remember only missing directories, so removal never deletes a pre-existing container.
pub fn record_created_dirs(vault: &Path, directories: &[PathBuf]) -> Result<(), Error> {
    let path = local_base(vault).join("backups/created-directories.json");
    let mut created: Vec<PathBuf> = read_file(&path)?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|e| Error::Config(e.to_string())))
        .transpose()?
        .unwrap_or_default();
    let before = created.clone();
    for directory in directories {
        for ancestor in directory.ancestors() {
            match std::fs::symlink_metadata(ancestor) {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if !created.iter().any(|p| p == ancestor) {
                        created.push(ancestor.to_path_buf());
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    if before != created {
        local_dir(vault)?;
        private_atomic(
            &path,
            &serde_json::to_vec(&created).map_err(|e| Error::Config(e.to_string()))?,
        )?;
    }
    Ok(())
}

/// Remove recorded empty containers; user files and other vaults' files always win.
pub fn cleanup_created_dirs(vault: &Path) -> Result<(), Error> {
    let path = local_base(vault).join("backups/created-directories.json");
    let Some(bytes) = read_file(&path)? else {
        return Ok(());
    };
    let mut created: Vec<PathBuf> =
        serde_json::from_slice(&bytes).map_err(|e| Error::Config(e.to_string()))?;
    created.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for directory in created {
        match std::fs::remove_dir(directory) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    std::fs::remove_file(path)?;
    Ok(())
}
