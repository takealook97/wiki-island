//! Identifier resolution, search and delivery.
use super::*;

impl Engine {
    /// The note an identifier names. Engine files under `.wiki-island/`
    /// (capture notes aside) are not notes: work items are delivered by `wi_work` and the brief
    /// by its resource, never by read, edit or context.
    pub(super) fn resolve(&self, identifier: &str) -> Result<ReadResult> {
        if wi_note::is_hidden_engine_path(identifier) {
            return Err(StoreError::Protected {
                path: identifier.to_owned(),
                reason: "engine file, not a note",
            }
            .into());
        }
        let read = self.resolve_any(identifier)?;
        if wi_note::is_hidden_engine_path(&read.path) {
            return Err(Error::NotFound);
        }
        Ok(read)
    }

    fn resolve_any(&self, identifier: &str) -> Result<ReadResult> {
        match self.vault.read(identifier) {
            Ok(r) => return Ok(r),
            Err(StoreError::NotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        // Identity resolution must use exact identity, never the first eight
        // ranked token matches (alpha must not disappear behind alpha-1..N).
        if let Ok(context) = self.vault.context(identifier, 0) {
            if context
                .diagnostics
                .iter()
                .any(|d| d.message == "context-root-ambiguous")
            {
                return Err(Error::Invalid(
                    "Ambiguous permalink. Use an exact file path.",
                ));
            }
            if let Some(hit) = context.hits.first() {
                let read = self.vault.read(&hit.rel_path.to_string_lossy())?;
                if read.checksum != hit.content_hash
                    || self.hit(&read, false)?.permalink != identifier
                {
                    return Err(Error::NotFound);
                }
                return Ok(read);
            }
            if context.index_as_of.is_some() {
                return Err(Error::NotFound);
            }
        }
        let q = Query {
            text: identifier.into(),
            mode: SearchMode::Permalink,
            limit: 10000,
            ..Query::default()
        };
        let matches = self
            .vault
            .search(&q, SearchOptions::default())?
            .hits
            .into_iter()
            .filter(|h| h.permalink == identifier)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(if matches.is_empty() {
                Error::NotFound
            } else {
                Error::Invalid("Ambiguous permalink. Use an exact file path.")
            });
        }
        let read = self.vault.read(&matches[0].rel_path.to_string_lossy())?;
        if read.checksum != matches[0].content_hash
            || self.hit(&read, false)?.permalink != identifier
        {
            return Err(Error::NotFound);
        }
        Ok(read)
    }

    pub(super) fn hit(&self, read: &ReadResult, fallback: bool) -> Result<DeliveryHit> {
        let mut content = std::str::from_utf8(&read.bytes)
            .map_err(|_| Error::Unsupported("Binary files are unavailable through note tools."))?
            .to_owned();
        let note = Note::parse(&read.bytes).map_err(StoreError::from)?;
        let metadata: serde_json::Map<String, Value> = if note
            .frontmatter()
            .is_some_and(|f| note.body_start() == f.byte_range.end)
        {
            note.frontmatter()
                .map(|f| {
                    f.keys()
                        .filter_map(|k| f.get(k).map(|v| (k.to_owned(), v)))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Default::default()
        };
        if note
            .frontmatter()
            .is_some_and(|f| note.body_start() == f.byte_range.end)
        {
            content = note.body().to_owned();
        }
        let mut metadata = Value::Object(metadata);
        let sources = metadata
            .as_object_mut()
            .and_then(|m| m.remove("sources"))
            .unwrap_or(Value::Null);
        let lifecycle = match note.lifecycle().lifecycle_type {
            LifecycleType::Memory => "memory",
            LifecycleType::Capture => "capture",
            LifecycleType::Work => "work",
            LifecycleType::None => "none",
        };
        Ok(DeliveryHit {
            path: read.path.clone(),
            permalink: metadata["permalink"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| generate_permalink(&read.path)),
            title: wi_note::hit_title(&read.path, &metadata, note.body()),
            content,
            file_hash: read.checksum.clone(),
            lifecycle: lifecycle.into(),
            status: metadata["status"].as_str().map(str::to_owned),
            claim_kind: metadata["claim_kind"].as_str().map(str::to_owned),
            sources,
            metadata,
            freshness: json!("unknown"),
            fallback,
            label: EVIDENCE_LABEL.into(),
        })
    }

    pub(super) fn deliver(&self, hits: Vec<DeliveryHit>, a: &Value, out: &mut Value) -> Result<()> {
        let conditions = a.get("conditions").unwrap_or(&Value::Null);
        let presented = self
            .lifecycle
            .present(&self.vault, hits, mode(a), conditions)?;
        for w in presented.withheld {
            result::withhold(out, &w.reason, w.count);
        }
        let mut hits = presented.hits;
        for hit in &mut hits {
            hit.label = EVIDENCE_LABEL.into();
        }
        out["hits"] = json!(hits);
        Ok(())
    }

    pub(super) fn hydrate(&self, hits: Vec<SearchHit>, a: &Value, out: &mut Value) -> Result<()> {
        let mut delivered = Vec::new();
        for hit in hits {
            match self.vault.read(&hit.rel_path.to_string_lossy()) {
                Ok(read) if read.checksum == hit.content_hash => {
                    delivered.push(self.hit(&read, hit.fallback)?)
                }
                _ => result::withhold(out, "changed-after-index", 1),
            }
        }
        self.deliver(delivered, a, out)
    }

    pub(super) fn search(&self, a: &Value, out: &mut Value) -> Result<()> {
        let query = Query {
            text: s(a, "query").unwrap_or("").into(),
            mode: match s(a, "mode") {
                Some("title") => SearchMode::Title,
                Some("permalink") => SearchMode::Permalink,
                _ => SearchMode::Text,
            },
            metadata_filters: a
                .get("filters")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            type_filter: owned(a, "note_type"),
            ..Query::default()
        };
        let categories = a
            .get("categories")
            .and_then(Value::as_array)
            .filter(|c| !c.is_empty());
        let (offset, limit) = pagination(a)?;
        let mut cache: HashMap<String, bool> = HashMap::new();
        let mut in_category = |h: &SearchHit| {
            let Some(categories) = categories else {
                return true;
            };
            let path = h.rel_path.to_string_lossy().into_owned();
            *cache.entry(path).or_insert_with_key(|path| {
                self.vault
                    .read(path)
                    .ok()
                    .filter(|read| read.checksum == h.content_hash)
                    .and_then(|read| Note::parse(&read.bytes).ok())
                    .is_some_and(|note| {
                        note.observations().iter().any(|o| {
                            categories
                                .iter()
                                .any(|c| c.as_str() == o.category.as_deref())
                        })
                    })
            })
        };
        let accept: Option<&mut dyn FnMut(&SearchHit) -> bool> = match categories {
            Some(_) => Some(&mut in_category),
            None => None,
        };
        let window = window::fill(&self.vault, query, offset, limit, accept)?;
        let page = window.page(offset, limit, out);
        self.hydrate(page, a, out)
    }
}
