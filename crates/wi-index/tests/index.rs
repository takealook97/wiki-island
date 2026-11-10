use serde_json::json;
use std::{fs, path::Path};
use tempfile::TempDir;
use wi_index::{Error, Index, Query, SearchMode, literal_search};

struct Fixture {
    vault: TempDir,
    cache: TempDir,
    index: Index,
}
impl Fixture {
    fn new() -> Self {
        let vault = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let index =
            Index::open_at(vault.path(), "test", cache.path().join("index.sqlite3")).unwrap();
        Self {
            vault,
            cache,
            index,
        }
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.vault.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn sync(&mut self) {
        self.index.sync(self.vault.path()).unwrap();
    }
    fn paths(&self, text: &str) -> Vec<String> {
        self.index
            .search(&Query::text(text))
            .unwrap()
            .hits
            .into_iter()
            .map(|h| h.rel_path.to_string_lossy().into_owned())
            .collect()
    }
}

#[test]
fn korean_two_syllable_and_english() {
    let mut f = Fixture::new();
    f.write(
        "payment.md",
        "---\ntitle: Billing\n---\n결제와 환불 정책. English payment processing.",
    );
    f.write("other.md", "Unrelated English material.");
    f.sync();
    for query in ["결제", "환불", "payment", "process"] {
        assert_eq!(f.paths(query), ["payment.md"]);
    }
    assert!(f.paths("absent").is_empty());
}
#[test]
fn inflection_is_a_ranked_hit() {
    let mut f = Fixture::new();
    f.write("refund.md", "환불 정책을 안내합니다.");
    f.write("other.md", "배송 안내입니다.");
    f.sync();
    assert_eq!(f.paths("환불 정책"), ["refund.md"]);
    // No all-terms match: ranked OR over script bigrams still finds the note
    // first. This is n-gram overlap, not morphology.
    assert_eq!(
        f.paths("환불 정책이").first().map(String::as_str),
        Some("refund.md")
    );
}
#[test]
fn offline_no_model_dependencies() {
    // This test contains no network or model setup. The runtime dependency tree
    // is separately recorded by the gate; it has no HTTP client/model runtime.
    let mut f = Fixture::new();
    f.write("offline.md", "offline 결제");
    f.sync();
    assert_eq!(f.paths("offline"), ["offline.md"]);
}
#[test]
fn delete_index_rebuild_equal_results() {
    let mut f = Fixture::new();
    f.write("a.md", "---\ntitle: Payment\n---\n결제 환불 payment");
    f.write("b.md", "payment and other refund details");
    f.sync();
    let before: Vec<_> = ["결제", "환불", "payment", "none", "*"]
        .map(|q| f.paths(q))
        .into();
    let db = f.cache.path().join("index.sqlite3");
    drop(f.index);
    fs::remove_file(&db).unwrap();
    f.index = Index::open_at(f.vault.path(), "test", &db).unwrap();
    f.index.rebuild().unwrap();
    let after: Vec<_> = ["결제", "환불", "payment", "none", "*"]
        .map(|q| f.paths(q))
        .into();
    assert_eq!(before, after);
}
#[test]
fn missing_index_literal_fallback_labelled() {
    let f = Fixture::new();
    f.write("fallback.md", "환불 policy");
    let result = f.index.search(&Query::text("환불")).unwrap();
    assert!(result.fallback);
    assert!(result.index_as_of.is_none());
    assert!(result.hits[0].fallback);
    let result = literal_search(f.vault.path(), &Query::text("policy")).unwrap();
    assert!(result.hits[0].fallback);
}
#[test]
fn filters_match_index_and_fallback() {
    let mut f = Fixture::new();
    f.write("match.md","---\ntitle: Alpha\ntype: memory\nstatus: active\npriority: 7\ntags: [security, oauth]\nschema:\n  confidence: 0.8\nowner: null\napproved: true\n---\n결제 payment");
    f.write("decoy.md","---\ntitle: Alpha\ntype: note\nstatus: archived\npriority: 2\ntags: [security]\nowner: someone\n---\n결제 payment");
    f.sync();
    for filters in [
        json!({"status":"active"}),
        json!({"priority":{"gt":5}}),
        json!({"schema.confidence":{"$between":[0.7,0.9]}}),
        json!({"tags":["security","oauth"]}),
        json!({"owner":null}),
        json!({"owner":{"exists":false}}),
        json!({"status":{"in":["active","candidate"]}}),
        json!({"approved":true}),
        json!({"priority":{"gte":"7","lte":7}}),
        json!({"tags":{"contains":"oauth"}}),
    ] {
        let query = Query {
            text: "결제".into(),
            metadata_filters: filters.as_object().unwrap().clone(),
            ..Query::default()
        };
        for result in [
            f.index.search(&query).unwrap(),
            literal_search(f.vault.path(), &query).unwrap(),
        ] {
            assert_eq!(result.hits.len(), 1, "{filters}");
            assert_eq!(result.hits[0].rel_path, Path::new("match.md"));
        }
    }
    let query = Query {
        type_filter: Some("memory".into()),
        ..Query::text("payment")
    };
    assert_eq!(f.index.search(&query).unwrap().hits.len(), 1);
    let hit = &f.index.search(&query).unwrap().hits[0];
    assert_eq!(hit.status.as_deref(), Some("active"));
    assert_eq!(hit.r#type, "memory");
}
#[test]
fn statwalk_nested_offline_edit_and_hash_verification() {
    let mut f = Fixture::new();
    f.write("nested/deep/note.md", "old words");
    f.sync();
    let first = f.index.search(&Query::text("old")).unwrap();
    assert!(first.index_as_of.is_some());
    assert!(f.index.verify_hit(&first.hits[0]).unwrap());
    f.write("nested/deep/note.md", "new offline refund words");
    assert!(!f.index.verify_hit(&first.hits[0]).unwrap());
    let report = f.index.sync(f.vault.path()).unwrap();
    assert_eq!(report.parsed, 1);
    assert_eq!(report.hashed, 1);
    assert_eq!(f.paths("refund"), ["nested/deep/note.md"]);
    let report = f.index.sync(f.vault.path()).unwrap();
    assert_eq!(report.hashed, 0);
    assert_eq!(report.parsed, 0);
    assert_eq!(report.unchanged, 1);
}
#[test]
fn sync_deletion_targeted_update_and_same_content_stat_change() {
    let mut f = Fixture::new();
    f.write("a.md", "old content");
    f.write("b.md", "second note");
    f.sync();
    f.write("a.md", "old content");
    let report = f.index.update_paths(&[Path::new("a.md")]).unwrap();
    assert_eq!(report.hashed, 1);
    assert_eq!(report.parsed, 0);
    f.write("a.md", "new content");
    f.index.update_paths(&["a.md"]).unwrap();
    assert_eq!(f.paths("new"), ["a.md"]);
    fs::remove_file(f.vault.path().join("b.md")).unwrap();
    let r = f.index.sync(f.vault.path()).unwrap();
    assert_eq!(r.removed, 1);
    assert!(f.paths("second").is_empty());
    assert_eq!(f.index.remove_paths(&["a.md"]).unwrap(), 1);
    assert!(f.paths("new").is_empty());
}
#[test]
fn ignores_local_git_nested_gitignore_and_config() {
    let mut f = Fixture::new();
    f.write(".git/private.md", "secret");
    f.write(".wiki-island/local/note.md", "secret");
    f.write(".wiki-island/config.toml", "ignore_dirs = [\"exports\"]\n");
    f.write("exports/note.md", "secret");
    f.write(".gitignore", "ignored/\n");
    f.write("ignored/note.md", "secret");
    f.write("nested/.gitignore", "skip.md\n");
    f.write("nested/skip.md", "secret");
    f.write("nested/keep.md", "visible");
    f.sync();
    assert!(f.paths("secret").is_empty());
    assert_eq!(f.paths("visible"), ["nested/keep.md"]);
    f.index
        .update_paths(&["ignored/note.md", "exports/note.md", "nested/skip.md"])
        .unwrap();
    assert!(f.paths("secret").is_empty());
}
#[test]
fn invalid_paths_queries_and_vault_identity_refused() {
    let mut f = Fixture::new();
    assert!(f.index.update_paths(&["../outside.md"]).is_err());
    assert!(f.index.remove_paths(&["/absolute.md"]).is_err());
    assert!(
        Index::open_at(
            f.vault.path(),
            "other",
            f.cache.path().join("index.sqlite3")
        )
        .is_err()
    );
    assert!(Index::open_at(f.vault.path(), "test", f.vault.path().join("index.sqlite3")).is_err());
    assert!(Index::cache_path("../bad").is_err());
    f.write("a.md", "content");
    f.sync();
    for filters in [
        json!({"bad..path":1}),
        json!({"x":{"unsupported":1}}),
        json!({"x":{"in":[]}}),
        json!({"x":{"gt":null}}),
        json!({"x":{"exists":1}}),
        json!({"x":{"contains":[]}}),
        json!({"x":{}}),
    ] {
        let q = Query {
            metadata_filters: filters.as_object().unwrap().clone(),
            ..Query::default()
        };
        assert!(matches!(f.index.search(&q), Err(Error::InvalidQuery(_))));
        assert!(literal_search(f.vault.path(), &q).is_err());
    }
    assert!(
        f.index
            .search(&Query::text("AND content"))
            .unwrap()
            .fallback
    );
    assert!(
        f.index
            .search(&Query::text("\"unterminated"))
            .unwrap()
            .fallback
    );
}
#[test]
fn schema_version_mismatch_resets_disposable_index() {
    let mut f = Fixture::new();
    f.write("a.md", "indexed");
    f.sync();
    let db = f.cache.path().join("index.sqlite3");
    drop(f.index);
    let c = rusqlite::Connection::open(&db).unwrap();
    c.pragma_update(None, "user_version", 999).unwrap();
    drop(c);
    f.index = Index::open_at(f.vault.path(), "test", db).unwrap();
    assert!(f.index.search(&Query::text("indexed")).unwrap().fallback);
    f.sync();
    assert!(!f.index.search(&Query::text("indexed")).unwrap().fallback);
}
#[test]
fn previous_cache_version_resets_and_reprojects_script_tokens() {
    let mut f = Fixture::new();
    f.write("a.md", "适者 生存");
    f.sync();
    let db = f.cache.path().join("index.sqlite3");
    drop(f.index);
    let c = rusqlite::Connection::open(&db).unwrap();
    c.pragma_update(None, "user_version", 6).unwrap();
    c.execute("UPDATE search_fts SET script_ngrams='stale-token'", [])
        .unwrap();
    drop(c);
    f.index = Index::open_at(f.vault.path(), "test", &db).unwrap();
    let c = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(
        c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
    assert_eq!(
        c.query_row("SELECT count(*) FROM search_fts", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(f.index.search(&Query::text("适者")).unwrap().fallback);
    f.sync();
    assert_eq!(f.paths("适者"), ["a.md"]);
    assert!(!f.index.search(&Query::text("适者")).unwrap().fallback);
    let terms: String = c
        .query_row("SELECT script_ngrams FROM search_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(terms, "适 者 适者 script_boundary 生 存 生存");
}

#[test]
fn current_and_legacy_sidecars_index_only_source_provenance() {
    for key in ["parse_semantics", wi_note::LEGACY_PARSE_SEMANTICS_KEY] {
        let mut f = Fixture::new();
        let source = json!({"kind":"file", "media_type":"application/pdf", "size_bytes":1,
            "entity_external_id":"22222222-3333-4444-5555-666666666666",
            "file_path":"survey.pdf", "checksum":format!("sha256:{}", "c".repeat(64))});
        let text = format!(
            "---\ntype: extracted_text\n{key}: false\nsource: {source}\n---\n- [fact] extracted content\n- relates_to [[Forged]]\n"
        );
        f.write("survey.pdf.md", &text);
        f.sync();
        assert_eq!(f.paths("extracted"), ["survey.pdf.md"]);
        let c = rusqlite::Connection::open(f.cache.path().join("index.sqlite3")).unwrap();
        assert_eq!(
            c.query_row("SELECT count(*) FROM observations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let edge: (String, String, i64) = c
            .query_row(
                "SELECT to_name,relation_type,target_file FROM relations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(edge, ("survey.pdf".into(), "derived_from".into(), 1));
        assert_eq!(
            c.query_row("SELECT count(*) FROM relations", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            std::fs::read(f.vault.path().join("survey.pdf.md")).unwrap(),
            text.as_bytes()
        );
    }
}

#[test]
fn rebuild_skips_invalid_file_and_updates_valid_files() {
    let mut f = Fixture::new();
    f.write("a.md", "original");
    f.sync();
    f.write("a.md", "changed");
    fs::write(f.vault.path().join("invalid.md"), [255, 254]).unwrap();
    let report = f.index.rebuild().unwrap();
    assert_eq!(report.skipped, [Path::new("invalid.md")]);
    assert_eq!(f.paths("changed"), ["a.md"]);
    assert!(f.paths("original").is_empty());
}
#[test]
fn context_resolves_forward_links_and_backlinks_without_cycles() {
    let mut f = Fixture::new();
    f.write(
        "a.md",
        "---\npermalink: a\ntitle: Alpha\n---\n- depends_on [[Beta]]\n- [[Missing]]\n",
    );
    f.write(
        "b.md",
        "---\npermalink: b\ntitle: Beta\n---\n[[Alpha|alias]]",
    );
    f.sync();
    let context = f.index.context("a", 2).unwrap();
    assert_eq!(context.hits.len(), 2);
    assert!(
        context
            .edges
            .iter()
            .any(|e| e.to_name == "Missing" && e.to_path.is_none())
    );
    assert!(context.edges.iter().any(
        |e| e.relation_type == "depends_on" && e.to_path.as_deref() == Some(Path::new("b.md"))
    ));
    assert_eq!(f.index.context("a", 0).unwrap().hits.len(), 1);
    f.write("c.md", "---\ntitle: Beta\n---\nother");
    f.index.update_paths(&["c.md"]).unwrap();
    assert!(
        f.index
            .context("a", 1)
            .unwrap()
            .edges
            .iter()
            .any(|e| e.to_name == "Beta" && e.to_path.is_none())
    );
}
#[cfg(unix)]
#[test]
fn symlinks_never_index_external_files() {
    use std::os::unix::fs::symlink;
    let mut f = Fixture::new();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("private.md"), "secret").unwrap();
    symlink(outside.path(), f.vault.path().join("link")).unwrap();
    f.sync();
    assert!(f.paths("secret").is_empty());
    assert!(f.index.update_paths(&["link/private.md"]).is_err());
}

// Script search covers order, punctuation, column scopes and ranking.
#[test]
fn script_cjk_substring_order_single_character_and_mixed_words() {
    let mut f = Fixture::new();
    f.write(
        "evolution.md",
        "---\ntitle: 進化について\n---\nOpenAI and 即适者生存的讨论与黑猫，時々更新",
    );
    f.sync();
    for q in [
        "适者生存",
        "OpenAI 适者生存",
        "OpenAI and 适者生存",
        "猫",
        "時々",
    ] {
        assert_eq!(f.paths(q), ["evolution.md"], "{q}");
    }
    assert!(f.paths("适生者存").is_empty());
    assert!(f.paths("missing 適者").is_empty());
}
#[test]
fn script_mixed_token_does_not_combine_distributed_terms() {
    let mut f = Fixture::new();
    f.write("match.md", "foo適者bar");
    f.write("decoy.md", "foo生存bar x適者y");
    f.sync();
    assert_eq!(f.paths("foo適者bar"), ["match.md"]);
}
#[test]
fn script_join_controls_keep_script_order() {
    for join in ["\u{200c}", "\u{200d}"] {
        let mut f = Fixture::new();
        f.write("match.md", &format!("ក{join}ខ"));
        f.write("reverse.md", &format!("ខ{join}ក"));
        f.sync();
        assert_eq!(f.paths(&format!("ក{join}ខ")), ["match.md"]);
    }
}
#[test]
fn script_katakana_extended_b_substring() {
    let mut f = Fixture::new();
    f.write("kana.md", "\u{1aff0}\u{1aff3}\u{1affd}");
    f.sync();
    assert_eq!(f.paths("\u{1aff3}"), ["kana.md"]);
}
#[test]
fn script_punctuation_does_not_match_every_row() {
    let mut f = Fixture::new();
    f.write("a.md", "ordinary searchable words");
    f.sync();
    assert!(f.paths("!!!").is_empty());
    assert!(f.paths("😀").is_empty());
}
#[test]
fn script_mixed_words_must_share_a_column() {
    let mut f = Fixture::new();
    f.write("match.md", "alpha beta 適者生存");
    f.write("decoy.md", "---\ntitle: alpha\n---\nbeta 適者生存");
    f.sync();
    // Same-column all-terms match ranks first; the ranked OR fill follows.
    assert_eq!(f.paths("alpha beta 適者生存"), ["match.md", "decoy.md"]);
}
#[test]
fn script_quoted_phrase_adjacency_and_boolean_not_across_columns() {
    let mut f = Fixture::new();
    f.write("adjacent.md", "OpenAI 适者生存 coffee");
    f.write(
        "separate.md",
        "---\ntitle: coffee\n---\nOpenAI words far from 适者生存 pour",
    );
    f.sync();
    assert_eq!(f.paths("\"OpenAI 适者生存\""), ["adjacent.md"]);
    assert_eq!(f.paths("coffee NOT pour"), ["adjacent.md"]);
    assert_eq!(f.paths("coffee OR tea").len(), 2);
}
#[test]
fn script_long_note_script_not_truncated() {
    let mut f = Fixture::new();
    f.write("long.md", &format!("{} 适者生存", "x".repeat(9_000)));
    f.sync();
    assert_eq!(f.paths("适者生存"), ["long.md"]);
}
#[test]
fn script_word_compatibility_bytes_preserved() {
    let mut f = Fixture::new();
    f.write("compat.md", "ＡＢＣ ﬁnance ＡＢＣ適者");
    f.sync();
    for q in ["ＡＢＣ ﬁnance", "ＡＢＣ適者"] {
        assert_eq!(f.paths(q), ["compat.md"]);
    }
}
#[test]
fn script_title_exact_match_first_and_title_scope() {
    let mut f = Fixture::new();
    f.write(
        "long.md",
        &format!("---\ntitle: Payment\n---\n{}", "long body ".repeat(100)),
    );
    f.write("short.md", "---\ntitle: Payment history\n---\nsmall");
    f.write("body.md", "payment");
    f.sync();
    let q = Query {
        mode: SearchMode::Title,
        ..Query::text("Payment")
    };
    let hits = f.index.search(&q).unwrap().hits;
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].rel_path, Path::new("long.md"));
}
#[test]
fn permalink_path_exact_glob_and_slash_token_boundary() {
    let mut f = Fixture::new();
    f.write("a.md", "---\npermalink: notes/payment\n---\na");
    f.write("b.md", "---\npermalink: notes/payment-extra\n---\nb");
    f.write("c.md", "---\npermalink: payment\n---\nc");
    f.sync();
    for (text, count) in [("notes/payment", 1), ("notes/payment*", 2), ("payment", 3)] {
        let q = Query {
            mode: SearchMode::Permalink,
            ..Query::text(text)
        };
        assert_eq!(f.index.search(&q).unwrap().hits.len(), count, "{text}");
    }
}
#[test]
fn script_mixed_search_bm25_ranking_includes_title_weight() {
    let mut f = Fixture::new();
    f.write(
        "strong.md",
        "---\ntitle: OpenAI OpenAI OpenAI\n---\nOpenAI 即适者生存",
    );
    f.write(
        "weak.md",
        "---\ntitle: Weaker match\n---\nOpenAI 即适者生存",
    );
    f.sync();
    let hits = f
        .index
        .search(&Query::text("OpenAI 适者生存"))
        .unwrap()
        .hits;
    assert_eq!(hits[0].rel_path, Path::new("strong.md"));
    assert!(hits.iter().all(|h| h.score != 0.0));
}

#[test]
fn corrupt_index_uses_literal_without_replacing_bytes() {
    let f = Fixture::new();
    f.write("a.md", "환불");
    let corrupt = f.cache.path().join("corrupt.sqlite3");
    fs::write(&corrupt, "not a database").unwrap();
    let result =
        wi_index::search_at_with_fallback(f.vault.path(), "test", &corrupt, &Query::text("환불"))
            .unwrap();
    assert!(result.fallback);
    assert!(result.hits[0].fallback);
    assert_eq!(fs::read(&corrupt).unwrap(), b"not a database");
    let missing = f.cache.path().join("missing.sqlite3");
    assert!(
        wi_index::search_at_with_fallback(f.vault.path(), "test", &missing, &Query::text("환불"))
            .unwrap()
            .fallback
    );
    assert!(!missing.exists());
}
#[test]
fn relative_markdown_relations_resolve_from_the_source_directory() {
    let mut f = Fixture::new();
    f.write(
        "nested/a.md",
        "---\npermalink: a\n---\n[Beta](b.md) and [Root](../root.md)",
    );
    f.write("nested/b.md", "beta");
    f.write("root.md", "root");
    f.sync();
    let context = f.index.context("a", 1).unwrap();
    assert_eq!(context.hits.len(), 3);
    assert!(
        context
            .edges
            .iter()
            .any(|e| e.to_path.as_deref() == Some(Path::new("nested/b.md")))
    );
}
#[test]
fn numeric_exponents_null_arrays_and_text_comparison_share_fallback_semantics() {
    let mut f = Fixture::new();
    f.write("typed.md","---\nfloat: 2.5\nprefix: 12abc\nflag: true\nvalues: [1, true, text]\nstarted: 2026-01-10\n---\nmatch");
    f.sync();
    for filters in [
        json!({"float":{"gt":1e-2}}),
        json!({"prefix":{"gt":10}}),
        json!({"flag":true}),
        json!({"values":{"contains":[1,true]}}),
        json!({"started":{"gte":"2026-01-01","lt":"2026-02-01"}}),
        json!({"missing":null}),
        json!({"float":{"exists":true}}),
    ] {
        let q = Query {
            metadata_filters: filters.as_object().unwrap().clone(),
            ..Query::text("match")
        };
        assert_eq!(f.index.search(&q).unwrap().hits.len(), 1, "{filters}");
        assert_eq!(
            literal_search(f.vault.path(), &q).unwrap().hits.len(),
            1,
            "{filters}"
        );
    }
    let q = Query {
        metadata_filters: json!({"float":{"gt":"9".repeat(400)}})
            .as_object()
            .unwrap()
            .clone(),
        ..Query::default()
    };
    assert!(f.index.search(&q).is_err());
}

#[test]
fn malformed_bom_frontmatter_is_plain_text_with_sync_diagnostics() {
    let mut f = Fixture::new();
    f.write(
        "bad.md",
        "\u{feff}---\nstatus: active\nstatus: rejected\n---\nreadable words",
    );
    let report = f.index.sync(f.vault.path()).unwrap();
    assert!(!report.diagnostics.is_empty());
    let hits = f.index.search(&Query::text("readable")).unwrap().hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].status, None);
    assert_eq!(hits[0].metadata, json!({}));
}
#[test]
fn readonly_queries_and_zero_limits_do_not_write_canonical_notes() {
    let mut f = Fixture::new();
    let text = "---\ncustom: 'Keep: exactly'\n---\n환불 [[Other]]\n";
    f.write("a.md", text);
    f.sync();
    f.index.search(&Query::text("환불")).unwrap();
    f.index.context("a", 1).unwrap();
    assert_eq!(
        fs::read_to_string(f.vault.path().join("a.md")).unwrap(),
        text
    );
    let q = Query {
        limit: 0,
        ..Query::text("환불")
    };
    assert!(f.index.search(&q).unwrap().hits.is_empty());
    assert!(literal_search(f.vault.path(), &q).unwrap().hits.is_empty());
}

#[test]
fn invalid_index_location_refusal_creates_nothing_inside_vault() {
    let f = Fixture::new();
    let inside = f.vault.path().join("new-cache/nested/index.sqlite3");
    assert!(Index::open_at(f.vault.path(), "test", inside).is_err());
    assert!(!f.vault.path().join("new-cache").exists());
}
#[test]
fn version_mismatch_still_refuses_a_different_vault_identity() {
    let f = Fixture::new();
    let db = f.cache.path().join("index.sqlite3");
    drop(f.index);
    let c = rusqlite::Connection::open(&db).unwrap();
    c.pragma_update(None, "user_version", 999).unwrap();
    drop(c);
    assert!(matches!(
        Index::open_at(f.vault.path(), "other", db),
        Err(Error::VaultMismatch)
    ));
}

#[test]
fn script_search_with_metadata_preserves_bm25_ranking() {
    let mut f = Fixture::new();
    f.write(
        "strong.md",
        "---\ntitle: 適者生存 適者生存 適者生存\nstatus: active\n---\nCLI metadata regression",
    );
    f.write(
        "weak.md",
        "---\ntitle: 適者生存\nstatus: active\n---\nCLI metadata regression",
    );
    f.write(
        "excluded.md",
        "---\ntitle: 適者生存\nstatus: archived\n---\nCLI metadata regression",
    );
    f.sync();
    let q = Query {
        metadata_filters: json!({"status":"active"}).as_object().unwrap().clone(),
        ..Query::text("適者生存")
    };
    let hits = f.index.search(&q).unwrap().hits;
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].rel_path, Path::new("strong.md"));
    assert!(hits[0].score < hits[1].score);
}

#[test]
fn slash_tokenizer_behavior_is_explicit() {
    for (tokenizer, expected) in [
        ("unicode61 tokenchars 0x2F", 1),
        ("unicode61 tokenchars '/'", 0),
    ] {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(&format!("CREATE VIRTUAL TABLE f USING fts5(permalink,tokenize=\"{tokenizer}\"); INSERT INTO f VALUES('notes/payment');")).unwrap();
        let count: i64 = c
            .query_row("SELECT count(*) FROM f WHERE f MATCH 'payment'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, expected);
    }
}
#[test]
fn script_prefix_phrases_file_paths_and_sentence_punctuation() {
    let mut f = Fixture::new();
    f.write("a.md", "alpha beta-long and notes/exact.md");
    f.write("b.md", "alpha beta-long and notes/exact.mdx");
    f.sync();
    assert_eq!(f.paths("\"alpha beta\"").len(), 2);
    assert_eq!(f.paths("alpha & beta").len(), 2);
    assert_eq!(f.paths("notes/exact.md"), ["a.md"]);
}

#[test]
fn search_watermark_and_metadata_share_a_sqlite_read_snapshot() {
    let mut f = Fixture::new();
    f.write("a.md", "match");
    f.sync();
    let db = f.cache.path().join("index.sqlite3");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection.execute_batch("BEGIN; UPDATE notes SET metadata='{\"state\":\"initial\"}'; UPDATE index_meta SET value='initial' WHERE key='index_as_of'; COMMIT;").unwrap();
    drop(connection);
    let writer = std::thread::spawn(move || {
        let mut c = rusqlite::Connection::open(db).unwrap();
        c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
        for i in 0..300 {
            let state = i.to_string();
            let tx = c.transaction().unwrap();
            tx.execute(
                "UPDATE notes SET metadata=?1",
                [json!({"state":state}).to_string()],
            )
            .unwrap();
            tx.execute(
                "UPDATE index_meta SET value=?1 WHERE key='index_as_of'",
                [state],
            )
            .unwrap();
            tx.commit().unwrap();
        }
    });
    for _ in 0..300 {
        let result = f.index.search(&Query::text("match")).unwrap();
        assert_eq!(
            result.index_as_of.as_deref(),
            result.hits[0]
                .metadata
                .get("state")
                .and_then(serde_json::Value::as_str)
        );
    }
    writer.join().unwrap();
}
#[test]
fn ranked_multi_term_ranks_all_terms_first_and_keeps_explicit_syntax_strict() {
    let mut f = Fixture::new();
    f.write("both.md", "임종석은 여의도 집회를 주도했다.");
    f.write("one.md", "여의도 공원 산책 기록.");
    f.write("none.md", "unrelated english words");
    f.sync();
    // Natural-language question: no note contains every term, but ranked OR
    // over n-grams returns the best overlap first and omits non-overlap.
    let paths = f.paths("임종석이 여의도 시위를 주도한 날은?");
    assert_eq!(paths.first().map(String::as_str), Some("both.md"));
    assert!(paths.contains(&"one.md".to_string()));
    assert!(!paths.contains(&"none.md".to_string()));
    // All-terms matches stay first even when bm25 would favor a short OR hit.
    assert_eq!(f.paths("여의도 주도"), ["both.md", "one.md"]);
    // Explicit Boolean and phrase syntax keeps strict semantics.
    assert_eq!(f.paths("여의도 AND 집회를"), ["both.md"]);
    assert_eq!(f.paths("\"여의도 공원\""), ["one.md"]);
    // Single short terms are unchanged.
    assert_eq!(f.paths("공원"), ["one.md"]);
}
/// every lifecycle note has one row; its materialized
/// snapshot equals the one parsed from the file; rows follow updates and
/// removals and are identical after the index is deleted and rebuilt.
#[test]
fn lifecycle_rows_match_files_and_survive_rebuild() {
    let mut f = Fixture::new();
    let memory = "---\ntype: memory\nstatus: active\nkind: fact\nsummary: s\nvalue: null\nsources: [{ref: a.md, commit: null, pending: true, source_hash: 'sha256:00'}]\n---\nbody";
    f.write("m/one.md", memory);
    f.write("m/two.md", "---\ntype: work\nwork_id: w1\n---\nwork body");
    f.write(
        "c.md",
        "---\ntype: capture\ncapture_status: pending\n---\ncapture",
    );
    f.write("plain.md", "---\ntitle: Plain\n---\nplain");
    f.write("broken.md", "---\ntype: memory\nstatus: [x\n---\nbroken");
    f.sync();
    let format = wi_lifecycle::MATERIALIZED_FORMAT;
    let check = |index: &Index, root: &Path, expected: &[&str]| {
        let rows = index.lifecycle_rows().unwrap().unwrap();
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, expected);
        let snapshots = index.lifecycle_snapshots(format, &paths).unwrap();
        let mut out = Vec::new();
        for row in &rows {
            let bytes = fs::read(root.join(&row.path)).unwrap();
            let parsed = wi_lifecycle::NoteSnapshot::from_bytes(row.path.as_str(), &bytes);
            assert_eq!(row.content_hash, wi_index::content_hash(&bytes));
            let (hash, text) = &snapshots[&row.path];
            assert_eq!(hash, &row.content_hash);
            let stored = wi_lifecycle::NoteSnapshot::from_materialized(&row.path, text);
            // `value: null` reads as an unresolved field: it round-trips.
            assert_eq!(stored, Some(parsed), "{}", row.path);
            out.push((row.path.clone(), row.content_hash.clone(), text.clone()));
        }
        out
    };
    let before = check(&f.index, f.vault.path(), &["c.md", "m/one.md", "m/two.md"]);
    assert!(
        f.index
            .lifecycle_snapshots("other-format", &["c.md", "m/one.md"])
            .unwrap()
            .is_empty()
    );
    f.write("m/two.md", "---\ntype: work\nwork_id: w2\n---\nchanged");
    fs::remove_file(f.vault.path().join("c.md")).unwrap();
    f.write(
        "plain.md",
        "---\ntype: memory\nstatus: candidate\n---\nnow lifecycle",
    );
    f.sync();
    let updated = check(
        &f.index,
        f.vault.path(),
        &["m/one.md", "m/two.md", "plain.md"],
    );
    assert_ne!(before[2].2, updated[1].2);
    let db = f.cache.path().join("index.sqlite3");
    drop(f.index);
    fs::remove_file(&db).unwrap();
    f.index = Index::open_at(f.vault.path(), "test", &db).unwrap();
    f.index.rebuild().unwrap();
    let rebuilt = check(
        &f.index,
        f.vault.path(),
        &["m/one.md", "m/two.md", "plain.md"],
    );
    assert_eq!(updated, rebuilt);
}

#[cfg(windows)]
#[test]
fn junctions_and_symlinks_never_index_external_files() {
    let mut f = Fixture::new();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("private.md"), "secret").unwrap();
    let output = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(f.vault.path().join("link"))
        .arg(outside.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::os::windows::fs::symlink_file(
        outside.path().join("private.md"),
        f.vault.path().join("file.md"),
    )
    .unwrap();
    f.sync();
    assert!(f.paths("secret").is_empty());
    assert!(f.index.update_paths(&[r"link\private.md"]).is_err());
    assert!(f.index.update_paths(&["file.md"]).is_err());
}

#[cfg(windows)]
#[test]
fn native_nested_paths_share_slash_keys_for_walk_update_and_remove() {
    let mut f = Fixture::new();
    f.write(r"a\b\c.md", "needle");
    f.sync();
    assert_eq!(f.paths("needle"), ["a/b/c.md"]);
    let literal = literal_search(f.vault.path(), &Query::text("needle")).unwrap();
    assert_eq!(literal.hits[0].rel_path.to_str().unwrap(), "a/b/c.md");
    f.write(r"a\b\c.md", "changed");
    f.index.update_paths(&[r"a\b\c.md"]).unwrap();
    assert!(f.paths("needle").is_empty());
    assert_eq!(f.paths("changed"), ["a/b/c.md"]);
    assert_eq!(f.index.remove_paths(&[r"a\b\c.md"]).unwrap(), 1);
    assert!(f.paths("changed").is_empty());
    f.sync();
    fs::write(f.vault.path().join(r"a\b\c.md"), [255, 254]).unwrap();
    let report = f.index.sync(f.vault.path()).unwrap();
    assert_eq!(report.skipped[0].to_str().unwrap(), "a/b/c.md");
    let result = f.index.search(&Query::text("changed")).unwrap();
    assert!(result.is_skipped(&result.hits[0]));
    f.write(r"a\b\c.md", "repaired");
    f.index.update_paths(&[r"a\b\c.md"]).unwrap();
    assert!(
        f.index
            .search(&Query::text("repaired"))
            .unwrap()
            .skipped
            .is_empty()
    );
}
