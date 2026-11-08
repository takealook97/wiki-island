use crate::{Error, MapEntry, MapStore, Result, TokenSource, client::Client, map_markdown};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

/// One note from an immutable committed snapshot, never a working-tree read.
#[derive(Debug, Clone)]
pub struct PublishNote {
    pub rel_path: String,
    pub bytes: Vec<u8>,
    pub permalink: String,
    pub title: String,
    pub archived: bool,
}

/// Must return the complete committed snapshot. Missing mapped notes are archived.
/// On an incomplete read, return an error instead of a partial snapshot.
pub trait NoteSource {
    fn notes(&self) -> Result<Vec<PublishNote>>;
}

#[derive(Debug, Clone)]
pub struct PublishOptions {
    pub parent_page_id: String,
    pub rate_limit: f64,
    pub dry_run: bool,
    pub timeout: Duration,
    pub max_retries: u32,
    /// Production must use HTTPS; plain HTTP is allowed only on loopback for tests.
    pub api_base_url: String,
}

impl PublishOptions {
    pub fn new(parent_page_id: impl Into<String>) -> Self {
        Self {
            parent_page_id: parent_page_id.into(),
            rate_limit: 3.0,
            dry_run: false,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            api_base_url: "https://api.notion.com/v1".into(),
        }
    }

    fn validate(&self) -> Result<()> {
        crate::client::validate_id(&self.parent_page_id)?;
        if !self.rate_limit.is_finite()
            || self.rate_limit <= 0.0
            || Duration::try_from_secs_f64(1.0 / self.rate_limit.min(3.0)).is_err()
            || self.timeout.is_zero()
            || self.max_retries > 10
        {
            return Err(Error::Options("rate, timeout, or retry count is invalid"));
        }
        // Restrict credentials to the production service or an explicit local mock.
        let base = self.api_base_url.trim_end_matches('/');
        let local = ["http://127.0.0.1:", "http://[::1]:"].iter().any(|prefix| {
            base.strip_prefix(prefix)
                .and_then(|s| s.strip_suffix("/v1"))
                .is_some_and(|s| s.parse::<u16>().is_ok_and(|p| p != 0))
        });
        if base != "https://api.notion.com/v1" && !local {
            return Err(Error::Options(
                "API address must be the publishing service or a loopback mock",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishFailure {
    pub permalink: String,
    pub reason: String,
}

#[derive(Debug, Default, Clone)]
pub struct PublishReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    pub archived: Vec<String>,
    pub failed: Vec<PublishFailure>,
    pub warnings: BTreeMap<String, Vec<String>>,
}

/// The caller serializes map access and holds no engine lock during this call.
/// Dry runs report planned transitions without reading tokens or writing the map.
pub fn publish(
    source: &dyn NoteSource,
    tokens: &dyn TokenSource,
    map: &mut MapStore,
    options: &PublishOptions,
) -> Result<PublishReport> {
    options.validate()?;
    map.check_parent(&options.parent_page_id)?;
    let notes = source.notes()?;
    let mut present = BTreeSet::new();
    for note in &notes {
        if note.permalink.is_empty() || !present.insert(note.permalink.clone()) {
            return Err(Error::Source(
                "snapshot has empty or duplicate permalinks".into(),
            ));
        }
    }
    let mut report = PublishReport::default();
    let mut client = None;
    let mut created = BTreeSet::new();
    let mut failed = BTreeSet::new();
    // Establish page identities before resolving mentions, including forward links.
    for note in notes.iter().filter(|n| !n.archived) {
        if map.get(&note.permalink).is_some() {
            continue;
        }
        let result: Result<()> = (|| {
            let mapped = map_markdown(&note.bytes, &BTreeMap::new())?;
            crate::client::validate_blocks(&mapped.blocks)?;
            crate::client::title_text(&note.title)?;
            if options.dry_run {
                return Ok(());
            }
            let client = get_client(&mut client, tokens, map, options)?;
            let page_id = client.create(&options.parent_page_id, &note.title)?;
            map.set(
                &note.permalink,
                MapEntry {
                    page_id,
                    content_hash: String::new(),
                    archived: false,
                },
            )
        })();
        match result {
            Ok(()) => {
                created.insert(note.permalink.clone());
            }
            Err(error) => {
                failed.insert(note.permalink.clone());
                report.failed.push(PublishFailure {
                    permalink: note.permalink.clone(),
                    reason: error.to_string(),
                });
            }
        }
    }
    let known = link_targets(&notes, map);
    for note in notes
        .iter()
        .filter(|n| !n.archived && !failed.contains(&n.permalink))
    {
        let result: Result<()> = (|| {
            let mapped = map_markdown(&note.bytes, &known)?;
            crate::client::validate_blocks(&mapped.blocks)?;
            crate::client::title_text(&note.title)?;
            let mut digest = Sha256::new();
            digest.update(b"wi-notion:1\0");
            digest.update(Sha256::digest(&note.bytes));
            digest.update(serde_json::to_vec(&(&note.title, &mapped.blocks))?);
            let hash = format!("{:x}", digest.finalize());
            report
                .warnings
                .insert(note.permalink.clone(), mapped.warnings);
            let entry = map.get(&note.permalink).cloned();
            if entry
                .as_ref()
                .is_some_and(|e| e.content_hash == hash && !e.archived)
            {
                report.unchanged.push(note.permalink.clone());
                return Ok(());
            }
            if !options.dry_run {
                let mut entry = entry.ok_or(Error::Response)?;
                // Record a live-page intent before restore, so a failed rebuild can
                // still be archived if the note disappears on the next snapshot.
                entry.archived = false;
                entry.content_hash.clear();
                map.set(&note.permalink, entry.clone())?;
                let client = get_client(&mut client, tokens, map, options)?;
                client.page(&entry.page_id, Some(&note.title), false)?;
                client.clear(&entry.page_id)?;
                client.append(&entry.page_id, &mapped.blocks)?;
                entry.content_hash = hash;
                entry.archived = false;
                map.set(&note.permalink, entry)?;
            }
            if created.contains(&note.permalink) {
                report.created.push(note.permalink.clone());
            } else {
                report.updated.push(note.permalink.clone());
            }
            Ok(())
        })();
        if let Err(error) = result {
            report.failed.push(PublishFailure {
                permalink: note.permalink.clone(),
                reason: error.to_string(),
            });
        }
    }
    let archived: BTreeSet<_> = notes
        .iter()
        .filter(|n| n.archived)
        .map(|n| n.permalink.clone())
        .collect();
    let stale: Vec<_> = map
        .entries()
        .iter()
        .filter(|(key, entry)| {
            !entry.archived && (!present.contains(*key) || archived.contains(*key))
        })
        .map(|(key, entry)| (key.clone(), entry.clone()))
        .collect();
    for (key, mut entry) in stale {
        let result: Result<()> = (|| {
            if !options.dry_run {
                // A lost archive acknowledgment must not leave a reusable live hash.
                entry.content_hash.clear();
                map.set(&key, entry.clone())?;
                get_client(&mut client, tokens, map, options)?.page(&entry.page_id, None, true)?;
                entry.archived = true;
                map.set(&key, entry)?;
            }
            report.archived.push(key.clone());
            Ok(())
        })();
        if let Err(error) = result {
            report.failed.push(PublishFailure {
                permalink: key,
                reason: error.to_string(),
            });
        }
    }
    Ok(report)
}

fn get_client<'a>(
    slot: &'a mut Option<Client>,
    tokens: &dyn TokenSource,
    map: &mut MapStore,
    options: &PublishOptions,
) -> Result<&'a mut Client> {
    if slot.is_none() {
        let client = Client::new(tokens.token()?, options)?;
        map.bind_parent(&options.parent_page_id)?;
        *slot = Some(client);
    }
    slot.as_mut().ok_or(Error::Response)
}

/// Wikilink targets that resolve to a published page: each live note is
/// named by its path, its path without `.md`, its permalink and its title,
/// as the vault index names notes. A name shared by two live notes is
/// ambiguous and stays plain text.
fn link_targets(notes: &[PublishNote], map: &MapStore) -> BTreeMap<String, String> {
    let mut names: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for note in notes.iter().filter(|n| !n.archived) {
        let path = note.rel_path.as_str();
        for name in [
            path,
            path.strip_suffix(".md").unwrap_or(path),
            note.permalink.as_str(),
            note.title.as_str(),
        ] {
            if !name.is_empty() {
                names.entry(name).or_default().insert(&note.permalink);
            }
        }
    }
    names
        .into_iter()
        .filter_map(|(name, permalinks)| {
            let mut permalinks = permalinks.into_iter();
            let (Some(permalink), None) = (permalinks.next(), permalinks.next()) else {
                return None;
            };
            let entry = map.get(permalink)?;
            Some((name.to_owned(), entry.page_id.clone()))
        })
        .collect()
}
