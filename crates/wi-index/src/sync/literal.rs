//! Literal fallback search.
use crate::{Query, Result, SearchMode, SearchResult, make_hit, query};
use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

use super::*;

/// Case-insensitive literal substring scan, with the same metadata/type predicates.
/// Text is literal, so Boolean/prefix/phrase syntax has no special meaning here.
pub fn literal_search(vault_root: impl AsRef<Path>, query: &Query) -> Result<SearchResult> {
    let predicates = query.validate()?;
    let root = fs::canonicalize(vault_root)?;
    let mut report = SyncReport::default();
    let settings = match settings(&root) {
        Ok(settings) => settings,
        Err(e) => {
            report.diagnostics.push(IndexDiagnostic {
                rel_path: wi_note::VAULT_CONFIG_PATH.into(),
                message: e.to_string(),
            });
            Settings {
                ignored: vec![PathBuf::from(".wiki-island/local")],
                max_note_bytes: DEFAULT_MAX_NOTE_BYTES,
            }
        }
    };
    let files = walk(&root, settings.ignored, &mut report);
    let needle = query.text.trim().to_lowercase();
    // Reuse exactly the indexed JSON predicates, without opening an index file.
    // This keeps SQLite numeric coercion and null behavior identical in fallback.
    let filter_db = Connection::open_in_memory()?;
    let mut filter_params = vec![SqlValue::Null];
    let conditions: Vec<_> = predicates
        .iter()
        .map(|p| p.sql(&mut filter_params))
        .collect();
    let filter_sql = format!(
        "SELECT coalesce(({}),0) FROM (SELECT ?1 AS metadata) n",
        if conditions.is_empty() {
            "1".into()
        } else {
            conditions.join(" AND ")
        }
    );
    let mut filter = filter_db.prepare(&filter_sql)?;
    let mut hits = Vec::new();
    let mut matched = 0usize;
    if query.limit == 0 && query.count_limit == 0 {
        return Ok(SearchResult {
            hits,
            index_as_of: None,
            fallback: true,
            skipped: report.skipped,
            diagnostics: report.diagnostics,
            matches: None,
        });
    }
    for (path, s) in files {
        // Engine input is indexed for the engine, never delivered as a note.
        if wi_note::is_hidden_engine_path(&path) {
            continue;
        }
        let item = match prepare_retry(&root, &path, &s, None, settings.max_note_bytes) {
            Ok(item) => item,
            Err(e) => {
                skip(&mut report, &path, e);
                continue;
            }
        };
        let Some(note) = item.note else {
            continue;
        };
        report
            .diagnostics
            .extend(note.diagnostics().iter().map(|d| IndexDiagnostic {
                rel_path: path.clone().into(),
                message: d.message.clone(),
            }));
        let metadata = metadata(&note);
        if query.type_filter.as_ref().is_some_and(|t| {
            metadata
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("note")
                != t
        }) {
            continue;
        }
        let (permalink, title) = names(&path, &metadata);
        let haystack = match query.mode {
            SearchMode::Text => format!("{title}\n{}", note.body()),
            SearchMode::Title => title.clone(),
            SearchMode::Permalink => permalink.clone(),
        }
        .to_lowercase();
        let found = if needle.is_empty() || needle == "*" {
            true
        } else if query.mode == SearchMode::Permalink && needle.contains('*') {
            query::star_match(&needle, &haystack)
        } else {
            memchr::memmem::find(haystack.as_bytes(), needle.as_bytes()).is_some()
        };
        if found {
            filter_params[0] = SqlValue::Text(metadata.to_string());
            if !filter.query_row(params_from_iter(&filter_params), |r| r.get::<_, bool>(0))? {
                continue;
            }
            matched += 1;
            if hits.len() < query.limit {
                hits.push(make_hit(
                    path,
                    permalink,
                    note.body(),
                    metadata,
                    item.hash,
                    0.0,
                    true,
                    &query.text,
                ));
            }
            if hits.len() == query.limit && matched > query.count_limit {
                break;
            }
        }
    }
    Ok(SearchResult {
        hits,
        index_as_of: None,
        fallback: true,
        skipped: report.skipped,
        diagnostics: report.diagnostics,
        matches: (query.count_limit > 0).then_some(matched),
    })
}
