//! Database storage and link resolution.
use crate::{Error, Result, script::build_script_ngrams};
use rusqlite::{Transaction, params};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use wi_note::{Note, generate_permalink};

use super::*;

pub(super) fn remove(tx: &Transaction<'_>, path: &str) -> Result<usize> {
    tx.prepare_cached(
        "DELETE FROM search_fts WHERE rowid IN (SELECT rowid FROM notes WHERE path=?1)",
    )?
    .execute([path])?;
    tx.prepare_cached("DELETE FROM lifecycle_snapshots WHERE path=?1")?
        .execute([path])?;
    Ok(tx
        .prepare_cached("DELETE FROM files WHERE path=?1")?
        .execute([path])?)
}
pub(super) fn metadata(note: &Note) -> Value {
    let mut map = Map::new();
    // Malformed frontmatter is plain text and must never become authoritative metadata.
    if let Some(fm) = note
        .frontmatter()
        .filter(|fm| note.body_start() == fm.byte_range.end)
    {
        for entry in &fm.entries {
            map.insert(entry.key.clone(), entry.value.clone());
        }
    }
    Value::Object(map)
}
pub(super) fn names(path: &str, metadata: &Value) -> (String, String) {
    let permalink = metadata
        .get("permalink")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| generate_permalink(path));
    let title = metadata
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            Path::new(path)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    (permalink, title)
}
pub(super) fn store(tx: &Transaction<'_>, item: Prepared) -> Result<()> {
    let size =
        i64::try_from(item.stamp.size).map_err(|_| Error::InvalidPath(item.path.clone().into()))?;
    if item.note.is_none() {
        tx.prepare_cached("UPDATE files SET mtime_ns=?2,size=?3 WHERE path=?1")?
            .execute(params![item.path, item.stamp.mtime_ns, size])?;
        return Ok(());
    }
    remove(tx, &item.path)?;
    tx.prepare_cached("INSERT INTO files(path,mtime_ns,size,content_hash) VALUES(?1,?2,?3,?4)")?
        .execute(params![item.path, item.stamp.mtime_ns, size, item.hash])?;
    if let Some(note) = item.note {
        let metadata = metadata(&note);
        let (permalink, title) = names(&item.path, &metadata);
        let note_type = metadata
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("note");
        tx.prepare_cached("INSERT INTO notes(path,permalink,title,note_type,content,metadata) VALUES(?1,?2,?3,?4,?5,?6)")?.execute(params![item.path,permalink,title,note_type,note.body(),metadata.to_string()])?;
        let rowid = tx.last_insert_rowid();
        tx.prepare_cached("INSERT INTO search_fts(rowid,title,content,script_ngrams,permalink) VALUES(?1,?2,?3,?4,?5)")?.execute(params![rowid,title,note.body(),build_script_ngrams(&[&title,note.body()]),permalink])?;
        // Every lifecycle note gets a row (the cold listing reads only this
        // table) holding its engine snapshot of these bytes when that
        // round-trips, so a cold current read need not re-parse each file.
        if wi_lifecycle::LIFECYCLE_TYPES.contains(&note_type) {
            let snapshot =
                wi_lifecycle::NoteSnapshot::from_note(item.path.as_str(), &note).materialize();
            tx.prepare_cached("INSERT OR REPLACE INTO lifecycle_snapshots(path,content_hash,format,snapshot) VALUES(?1,?2,?3,?4)")?
                .execute(params![item.path, item.hash, wi_lifecycle::MATERIALIZED_FORMAT, snapshot])?;
        }
        for observation in note.observations() {
            tx.prepare_cached("INSERT INTO observations(path,category,content,tags,context,line_start,line_end) VALUES(?1,?2,?3,?4,?5,?6,?7)")?.execute(params![item.path,observation.category,observation.content,serde_json::to_string(&observation.tags)?,observation.context,observation.line_range.start as i64,observation.line_range.end as i64])?;
        }
        let mut relations: BTreeSet<_> = note
            .relations()
            .into_iter()
            .map(|r| (r.target, r.rel_type))
            .collect();
        for link in note.wikilinks() {
            // The parser's relation target retains aliases. Add the split identity only
            // if an equivalent explicit relation is absent.
            if !relations
                .iter()
                .any(|(target, _)| target.split('|').next() == Some(link.target.as_str()))
            {
                relations.insert((link.target, "links_to".into()));
            }
        }
        // Extracted text is opaque semantic input. Its sole provenance edge
        // comes from the source record, never from text that resembles a link.
        if matches!(note_type, "extracted_text" | "document")
            && metadata
                .get("parse_semantics")
                .or_else(|| metadata.get(wi_note::LEGACY_PARSE_SEMANTICS_KEY))
                == Some(&Value::Bool(false))
            && let Some(source) = metadata.get("source")
            && source.get("kind").and_then(Value::as_str) == Some("file")
            && source.get("size_bytes").and_then(Value::as_u64).is_some()
            && source
                .get("media_type")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains('/'))
            && source
                .get("entity_external_id")
                .and_then(Value::as_str)
                .is_some_and(|s| {
                    s.len() == 36
                        && s.bytes().enumerate().all(|(i, b)| {
                            if [8, 13, 18, 23].contains(&i) {
                                b == b'-'
                            } else {
                                b.is_ascii_hexdigit()
                            }
                        })
                })
            && let Some(path) = source.get("file_path").and_then(Value::as_str)
            && !path.starts_with('/')
            && !path.contains('\\')
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && item.path == format!("{path}.md")
            && source
                .get("checksum")
                .and_then(Value::as_str)
                .is_some_and(|s| {
                    s.strip_prefix("sha256:").is_some_and(|h| {
                        h.len() == 64
                            && h.bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
                })
        {
            // The target is the original binary, not a note: pin it to that file
            // so identity resolution never maps it onto the sidecar's permalink.
            tx.prepare_cached("INSERT OR IGNORE INTO relations(from_path,to_name,to_path,relation_type,relative_target,target_file) VALUES(?1,?2,?2,'derived_from',NULL,1)")?
                .execute(params![item.path, path])?;
        }
        for (target, kind) in relations {
            let target = target
                .split('|')
                .next()
                .unwrap_or(&target)
                .split('#')
                .next()
                .unwrap_or("");
            tx.prepare_cached(
                "INSERT OR IGNORE INTO relations(from_path,to_name,relation_type,relative_target) VALUES(?1,?2,?3,?4)",
            )?
            .execute(params![item.path, target, kind, relative_target(&item.path, target)])?;
        }
    }
    Ok(())
}
// Persist normalized relative targets so additions/deletions can find backlinks
// without scanning every relation in the vault.
pub(super) fn relative_target(from: &str, target: &str) -> Option<String> {
    if !target.starts_with("./") && !target.starts_with("../") {
        return None;
    }
    let mut parts: Vec<_> = from.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => (),
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}
pub(super) fn affected_names(tx: &Transaction<'_>, paths: &[String]) -> Result<BTreeSet<String>> {
    let mut names: BTreeSet<_> = paths.iter().cloned().collect();
    let mut stmt = tx.prepare_cached("SELECT permalink,title FROM notes WHERE path=?1")?;
    for path in paths {
        for row in stmt.query_map([path], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (permalink, title) = row?;
            names.insert(permalink);
            names.insert(title);
        }
    }
    Ok(names)
}
pub(super) fn resolve_affected(
    tx: &Transaction<'_>,
    paths: &[String],
    names: &BTreeSet<String>,
) -> Result<()> {
    let mut ids = BTreeSet::new();
    for path in paths {
        let mut stmt = tx.prepare_cached("SELECT rowid FROM relations WHERE from_path=?1 AND target_file=0 UNION SELECT rowid FROM relations WHERE relative_target=?1 AND target_file=0")?;
        ids.extend(
            stmt.query_map([path], |r| r.get::<_, i64>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
    }
    let mut stmt =
        tx.prepare_cached("SELECT rowid FROM relations WHERE to_name=?1 AND target_file=0")?;
    for name in names {
        ids.extend(
            stmt.query_map([name], |r| r.get::<_, i64>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
    }
    let mut relation =
        tx.prepare_cached("SELECT to_name,relative_target,to_path FROM relations WHERE rowid=?1")?;
    let mut lookup = tx.prepare_cached("SELECT path FROM notes WHERE path=?1 UNION SELECT path FROM notes WHERE permalink=?1 UNION SELECT path FROM notes WHERE title=?1 UNION SELECT path FROM notes WHERE path=?2")?;
    let mut update = tx.prepare_cached("UPDATE relations SET to_path=?1 WHERE rowid=?2")?;
    for id in ids {
        let (target, relative, old) = relation.query_row([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let candidates = lookup
            .query_map(params![target, relative], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let resolved = if candidates.len() == 1 {
            candidates.into_iter().next()
        } else {
            None
        };
        if resolved != old {
            update.execute(params![resolved, id])?;
        }
    }
    Ok(())
}
pub(super) fn resolve(tx: &Transaction<'_>) -> Result<()> {
    // Build each identity lookup once. Ambiguous authored identities remain
    // unresolved; adding/removing a target re-resolves existing backlinks.
    let mut identities: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut paths = BTreeSet::new();
    let mut notes = tx.prepare_cached("SELECT path,permalink,title FROM notes")?;
    for row in notes.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (path, permalink, title) = row?;
        paths.insert(path.clone());
        for key in [path.clone(), permalink, title] {
            identities.entry(key).or_default().insert(path.clone());
        }
    }
    let mut relations = tx.prepare_cached(
        "SELECT rowid,from_path,to_name,to_path FROM relations WHERE target_file=0",
    )?;
    let rows = relations
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut update = tx.prepare_cached("UPDATE relations SET to_path=?1 WHERE rowid=?2")?;
    for (id, from, target, old) in rows {
        let mut candidates = identities.get(&target).cloned().unwrap_or_default();
        if let Some(relative) = relative_target(&from, &target)
            && paths.contains(&relative)
        {
            candidates.insert(relative);
        }
        let resolved = if candidates.len() == 1 {
            candidates.into_iter().next()
        } else {
            None
        };
        if old != resolved {
            update.execute(params![resolved, id])?;
        }
    }
    Ok(())
}
