//! Directory listing and pagination.
use super::*;

impl Engine {
    pub(super) fn paths(&self, path: &str, depth: usize) -> Result<Vec<(String, bool)>> {
        // Traversal starts only at the canonical root. A relative subtree is
        // selected lexically, so neither .. nor a symlink can escape it.
        let prefix = folder(path)?;
        let mut paths = Vec::new();
        let root = self.vault.root().to_path_buf();
        let mut walk = ignore::WalkBuilder::new(&root);
        walk.hidden(false)
            .parents(false)
            .git_global(false)
            .follow_links(false)
            .filter_entry(move |e| {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if std::fs::symlink_metadata(e.path())
                        .is_ok_and(|m| m.file_attributes() & 0x400 != 0)
                    {
                        return false;
                    }
                }
                e.path().strip_prefix(&root).is_ok_and(|relative| {
                    !relative.components().any(
                        |c| matches!(c, Component::Normal(p) if p==".git" || p==".wiki-island"),
                    )
                }) && !e
                    .file_name()
                    .to_str()
                    .is_some_and(wi_store::is_scratch_name)
            });
        for entry in walk.build() {
            let Ok(entry) = entry else {
                continue;
            };
            if entry.depth() == 0 || entry.file_type().is_none_or(|t| t.is_symlink()) {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(self.vault.root()) else {
                continue;
            };
            let Some(rel) = rel.to_str() else {
                continue;
            };
            #[cfg(windows)]
            let rel = rel.replace('\\', "/");
            #[cfg(windows)]
            let rel = rel.as_str();
            let tail = if prefix.is_empty() {
                rel
            } else if let Some(tail) = rel.strip_prefix(&format!("{prefix}/")) {
                tail
            } else {
                continue;
            };
            if tail.split('/').count() > depth {
                continue;
            }
            paths.push((rel.into(), entry.file_type().is_some_and(|t| t.is_dir())));
        }
        paths.sort();
        Ok(paths)
    }

    pub(super) fn list(&self, name: &str, a: &Value, out: &mut Value) -> Result<()> {
        if a.get("name").is_some() && a.get("filters").is_some() {
            return Err(Error::Invalid("name cannot combine with filters."));
        }
        if a.get("fields").is_some() && a.get("filters").is_none() {
            return Err(Error::Invalid("fields needs filters."));
        }
        if let Some(filters) = a.get("filters") {
            if filters.as_object().is_none_or(|f| f.is_empty()) {
                return Err(Error::Invalid("filters must not be empty."));
            }
            // Apply the subtree before the delivery cap. Every selected hit
            // is re-read and hash-checked even if it was beyond the store's
            // first eight verified search rows.
            let prefix = folder(s(a, "path").unwrap_or("."))?;
            let query = Query {
                metadata_filters: filters.as_object().cloned().unwrap_or_default(),
                ..Query::default()
            };
            let (offset, limit) = pagination(a)?;
            let mut in_folder = |h: &SearchHit| {
                h.rel_path
                    .to_string_lossy()
                    .starts_with(&format!("{prefix}/"))
            };
            let accept: Option<&mut dyn FnMut(&SearchHit) -> bool> = if prefix.is_empty() {
                None
            } else {
                Some(&mut in_folder)
            };
            let window = window::fill(&self.vault, query, offset, limit, accept)?;
            let matches = window.page(offset, limit, out);
            self.hydrate(matches, a, out)?;
            if let Some(fields) = a["fields"].as_array()
                && let Some(hits) = out["hits"].as_array_mut()
            {
                for hit in hits {
                    let selected: serde_json::Map<String, Value> = fields
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|k| {
                            let value = k
                                .split('.')
                                .fold(&hit["metadata"], |v, part| &v[part])
                                .clone();
                            (k.to_owned(), value)
                        })
                        .collect();
                    // A lifecycle projection still carries complete qualifiers.
                    hit["selected_fields"] = json!(selected);
                    if hit["lifecycle"] == "none" {
                        hit["content"] = json!("");
                        hit["metadata"] = json!(selected);
                    }
                }
            }
            return Ok(());
        }
        let glob = s(a, if name == "wi_list" { "glob" } else { "name" })
            .map(|p| globset::Glob::new(p).map(|g| g.compile_matcher()))
            .transpose()
            .map_err(|_| Error::Invalid("Invalid basename glob."))?;
        let paths = self.paths(
            s(a, "path").unwrap_or("."),
            n(a, "depth", if name == "wi_list" { 1 } else { 10 }),
        )?;
        let order = s(a, "sort").unwrap_or("title_asc");
        let by_updated = order.starts_with("updated");
        let (offset, limit) = pagination(a)?;
        let folders_only = name == "wi_list"
            && offset.saturating_add(limit) <= paths.iter().filter(|(_, dir)| *dir).count();
        let indexed: HashMap<_, _> = if by_updated || folders_only {
            HashMap::new()
        } else {
            self.vault
                .name_rows()
                .ok()
                .flatten()
                .unwrap_or_default()
                .into_iter()
                .map(|row| (row.path.clone(), row))
                .collect()
        };
        let mut rows = Vec::new();
        let mut loaded = HashSet::new();
        let mut updated = HashMap::new();
        for (path, dir) in paths {
            if dir && name == "wi_find" {
                continue;
            }
            if !dir
                && glob
                    .as_ref()
                    .is_some_and(|g| !g.is_match(Path::new(&path).file_name().unwrap_or_default()))
            {
                continue;
            }
            if dir {
                if by_updated {
                    updated.insert(path.clone(), modified(self.vault.root(), &path));
                }
                rows.push(
                    json!({"path":path,"kind":"folder","size":null,"title":null,"permalink":null}),
                );
                continue;
            }
            let Ok(stat) = self.vault.name_stat(&path) else {
                result::withhold(out, "unsupported-file", 1);
                continue;
            };
            if by_updated || folders_only {
                if by_updated {
                    updated.insert(path.clone(), stat.modified);
                }
                // File names cannot affect a folder-only page or timestamp
                // sorting. Load fields only when a file reaches the page.
                rows.push(json!({"path":path,"kind":"file","size":stat.size}));
            } else if let Some(row) = indexed
                .get(&path)
                .filter(|row| row.size == stat.size && row.mtime_ns == stat.mtime_ns)
            {
                rows.push(json!({"path":path,"kind":"file","size":stat.size,
                    "title":row.title,"permalink":row.permalink.clone().unwrap_or_else(|| generate_permalink(&path))}));
            } else {
                // A changed, unsupported or ambiguous index row must retain
                // the original parser's fields before title sorting.
                match self.name_item(&path) {
                    Ok(row) => {
                        loaded.insert(path);
                        rows.push(row);
                    }
                    Err(_) => result::withhold(out, "unsupported-file", 1),
                }
            }
        }
        rows.sort_by(|a, b| {
            let folders = (a["kind"] != "folder").cmp(&(b["kind"] != "folder"));
            let key = |v: &Value| {
                v["title"]
                    .as_str()
                    .or_else(|| v["path"].as_str())
                    .unwrap_or("")
                    .to_lowercase()
            };
            let compared = if order.starts_with("updated") {
                updated
                    .get(a["path"].as_str().unwrap_or(""))
                    .cmp(&updated.get(b["path"].as_str().unwrap_or("")))
            } else {
                key(a).cmp(&key(b))
            };
            folders
                .then(if order.ends_with("desc") {
                    compared.reverse()
                } else {
                    compared
                })
                .then_with(|| a["path"].as_str().cmp(&b["path"].as_str()))
        });
        let mut total = rows.len();
        let mut items = Vec::new();
        for row in rows.into_iter().skip(offset) {
            if items.len() == limit {
                break;
            }
            let path = row["path"].as_str().unwrap_or_default();
            if row["kind"] == "folder" || loaded.contains(path) {
                items.push(row);
            } else {
                // Re-read only delivered files, including cached name rows.
                // A failed read is omitted and the next candidate fills it.
                match self.name_item(path) {
                    Ok(row) => items.push(row),
                    Err(_) => {
                        total -= 1;
                        result::withhold(out, "unsupported-file", 1);
                    }
                }
            }
        }
        page_meta(total, offset, limit, out);
        out["items"] = json!(items);
        Ok(())
    }

    fn name_item(&self, path: &str) -> Result<Value> {
        let read = match self.vault.read_bounded(path) {
            Ok(read) => read,
            Err(StoreError::TooLarge { .. }) => {
                // Above the read limit: the name stays discoverable from its
                // metadata, without loading the body (as for an unparsable file).
                let stat = self.vault.name_stat(path)?;
                return Ok(json!({"path":path,"kind":"file","size":stat.size,
                    "title":null,"permalink":generate_permalink(path)}));
            }
            Err(e) => return Err(e.into()),
        };
        // Binary and oversized files still have discoverable names; bodies
        // and lifecycle qualifiers are delivered only by wi_read.
        let note = Note::parse(&read.bytes).ok();
        let field = |key: &str| {
            note.as_ref()
                .and_then(|n| n.frontmatter())
                .and_then(|f| f.get(key))
                .and_then(|v| v.as_str().map(str::to_owned))
        };
        let metadata: Value = note
            .as_ref()
            .and_then(|n| n.frontmatter())
            .map(|f| {
                f.keys()
                    .filter_map(|k| f.get(k).map(|v| (k.to_owned(), v)))
                    .collect::<serde_json::Map<_, _>>()
                    .into()
            })
            .unwrap_or_default();
        let title = note
            .as_ref()
            .map(|n| wi_note::hit_title(path, &metadata, n.body()));
        Ok(json!({"path":path,"kind":"file","size":read.bytes.len(),
            "title":title,"permalink":field("permalink").unwrap_or_else(|| generate_permalink(path))}))
    }
}
