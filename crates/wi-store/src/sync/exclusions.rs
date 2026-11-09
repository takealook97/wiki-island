//! Oversized-file excludes.

use wi_history::{PublisherLock, SyncRepo, durable};

use crate::error::Result;
use crate::vault::Vault;
use crate::work::{WorkKind, WorkRequest};

use super::*;

impl Vault {
    pub(super) fn exclude_large_files(&self) -> Result<Vec<String>> {
        self.release_large_excludes(None)?;
        let mut large = Vec::new();
        for path in crate::ingest::walk_files(self.root()) {
            if std::fs::metadata(self.root().join(&path)).is_ok_and(|m| m.len() > LIMIT) {
                large.push(path);
            }
        }
        // Tracked files that grew past the limit are reported too; their
        // working bytes are never replaced by a remote version.
        if let Some(head) = SyncRepo::head(&self.history)? {
            for path in SyncRepo::tree(&self.history, Some(&head))?.keys() {
                if std::fs::metadata(self.root().join(path)).is_ok_and(|m| m.len() > LIMIT)
                    && !large.contains(path)
                {
                    large.push(path.clone());
                }
            }
        }
        if large.is_empty() {
            return Ok(large);
        }
        self.persist_large_excludes(&large)?;
        Ok(large)
    }

    /// Watcher and CLI ingress never commit a file larger than the limit,
    /// in either storage mode and with or without a remote: the file stays
    /// in the folder, gets a managed exclude rule and is reported once.
    pub(crate) fn sync_exclude_oversize(&self, rel: &str) -> Result<bool> {
        if !std::fs::metadata(self.root().join(rel)).is_ok_and(|m| m.len() > LIMIT) {
            return Ok(false);
        }
        self.persist_large_excludes(&[rel.to_owned()])?;
        Ok(true)
    }

    /// Drop the managed large-file rule of every path in `paths` (all
    /// managed rules when `None`) whose file is back at or under the
    /// limit, so a note that shrank is ingested and synced again. Returns
    /// the released paths.
    pub(crate) fn release_large_excludes(&self, paths: Option<&[&str]>) -> Result<Vec<String>> {
        let path = self.history.exclude_file();
        let shrunk = |text: &str| -> Vec<String> {
            let mut out = Vec::new();
            let mut lines = text.lines().peekable();
            while let Some(line) = lines.next() {
                if line != LARGE_MARK {
                    continue;
                }
                let Some(rule) = lines.peek().and_then(|r| r.strip_prefix('/')) else {
                    continue;
                };
                let rel = unescape_pattern(rule);
                let wanted = paths.is_none_or(|p| p.contains(&rel.as_str()));
                if wanted
                    && std::fs::metadata(self.root().join(&rel))
                        .is_ok_and(|m| m.is_file() && m.len() <= LIMIT)
                {
                    out.push(rel);
                }
            }
            out
        };
        // Cheap unlocked look first: most calls find nothing to release.
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if shrunk(&text).is_empty() {
            return Ok(Vec::new());
        }
        let _guard = PublisherLock::acquire(&self.history)?;
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let released = shrunk(&text);
        if released.is_empty() {
            return Ok(released);
        }
        let rules: Vec<String> = released
            .iter()
            .map(|rel| format!("/{}", escape_pattern(rel)))
            .collect();
        let mut kept = Vec::new();
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            if line == LARGE_MARK && lines.peek().is_some_and(|r| rules.iter().any(|x| x == r)) {
                lines.next();
                continue;
            }
            kept.push(line);
        }
        let mut out = kept.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        durable::write_atomic(&path, out.as_bytes(), self.history.durability())?;
        wi_index::set_exclude_file(self.root(), Some(&path));
        Ok(released)
    }

    pub(crate) fn persist_large_excludes(&self, large: &[String]) -> Result<()> {
        let _guard = PublisherLock::acquire(&self.history)?;
        let path = self.history.exclude_file();
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut reported = Vec::new();
        for rel in large {
            let rule = format!("/{}", escape_pattern(rel));
            if !text.lines().any(|l| l == rule) {
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(LARGE_MARK);
                text.push('\n');
                text.push_str(&rule);
                text.push('\n');
                reported.push(rel.clone());
            }
        }
        if reported.is_empty() {
            return Ok(());
        }
        // Record before excluding: a crash can repeat the deduplicated
        // request, but cannot hide a large file before its report is durable.
        for rel in &reported {
            crate::work::record(
                &self.history,
                &WorkRequest::new(
                    WorkKind::IngestFailed,
                    rel,
                    "This file is larger than 50 MB; it stays in the folder and is kept out of history and remote sync.",
                ),
            )?;
        }
        crate::fault::abort_at("sync_after_exclude_request");
        durable::write_atomic(&path, text.as_bytes(), self.history.durability())?;
        wi_index::set_exclude_file(self.root(), Some(&path));
        crate::fault::abort_at("sync_after_exclude");
        Ok(())
    }
}

fn escape_pattern(path: &str) -> String {
    path.chars()
        .flat_map(|c| {
            if matches!(c, '\\' | '*' | '?' | '[' | ']' | '!' | '#' | ' ') {
                vec!['\\', c]
            } else {
                vec![c]
            }
        })
        .collect()
}

/// Separate background sync owner. The watcher and maintenance never wait
fn unescape_pattern(rule: &str) -> String {
    let mut out = String::new();
    let mut chars = rule.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            other => out.push(other),
        }
    }
    out
}
