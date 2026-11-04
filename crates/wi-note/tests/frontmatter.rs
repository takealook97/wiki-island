use proptest::prelude::*;
use serde_json::json;
use std::path::Path;
use wi_note::*;

fn walk(path: &Path, count: &mut usize) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, count);
        } else if path.extension().is_some_and(|e| e == "md") {
            let bytes = std::fs::read(&path).unwrap();
            let note = Note::parse(&bytes).unwrap();
            assert!(
                note.diagnostics().is_empty(),
                "{}: {:?}",
                path.display(),
                note.diagnostics()
            );
            assert_eq!(note.to_bytes(), bytes, "{}", path.display());
            *count += 1;
        }
    }
}

#[test]
fn authored_fixture_corpus_round_trip() {
    let mut count = 0;
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/notes"),
        &mut count,
    );
    assert_eq!(count, 80, "fixture count: {count}");
}

#[test]
fn edge_bytes_round_trip() {
    for text in [
        "",
        "plain",
        "---\n---\n",
        "---\r\ntitle: test\r\n---\r\nbody\r\n",
        "\u{feff}---\ntitle: test\n---\nbody",
        "---\n# comment\ntitle: 'quoted' # keep\n---\n한국어 내용",
        "---\ntitle: test\n---",
        "body\0has nulls",
    ] {
        assert_eq!(
            Note::parse(text.as_bytes()).unwrap().to_bytes(),
            text.as_bytes()
        );
    }
}

// Measurement 3e's original temp directory was deleted. This reconstructs its
// recorded exact metadata bytes and adds a generic body; it is not the lost file.
const MEASUREMENT_3E: &str = "---\ntitle: Payment\ntype: note\ncustom_key: \"Keep: exactly\"\nunknown_list: [z, a]\n---\n# Payment\n\nA claim (beta, EU only).\n";

#[test]
fn only_touched_frontmatter_key_changes() {
    let note = Note::parse(MEASUREMENT_3E.as_bytes()).unwrap();
    assert!(note.diagnostics().is_empty(), "{:?}", note.diagnostics());
    let edited = set_key(&note, "title", &json!("Updated")).unwrap();
    assert_eq!(
        edited.to_bytes(),
        MEASUREMENT_3E
            .replacen("title: Payment", "title: Updated", 1)
            .as_bytes()
    );
    assert!(edited.frontmatter().unwrap().get("permalink").is_none());
    assert_eq!(note.to_bytes(), MEASUREMENT_3E.as_bytes());
}

#[test]
fn remove_add_and_adjacent_comments() {
    let text = "---\n# header\ntitle: Original # inline\n# between\nitems:\n  - one\n  - two\n\n# footer\n---\nBody\n";
    let note = Note::parse(text.as_bytes()).unwrap();
    let removed = remove_key(&note, "items").unwrap();
    assert_eq!(
        removed.to_bytes(),
        text.replace("items:\n  - one\n  - two\n", "").as_bytes()
    );
    let added = set_key(
        &note,
        "new",
        &json!(["yes",false, {"quote": "Keep: exactly"}]),
    )
    .unwrap();
    assert!(
        std::str::from_utf8(added.to_bytes())
            .unwrap()
            .starts_with(&text[..text.rfind("---\nBody").unwrap()])
    );
    assert_eq!(added.body(), "Body\n");
    assert_eq!(
        added.frontmatter().unwrap().get("new"),
        Some(json!(["yes",false,{"quote":"Keep: exactly"}]))
    );
    assert_eq!(
        remove_key(&note, "missing").unwrap().to_bytes(),
        note.to_bytes()
    );
}

#[test]
fn multiline_value_spans_do_not_swallow_following_comments() {
    let text =
        "---\nsummary: |\n  First\n  # part of the value\n  Last\n# keep\nnext: unchanged\n---\n";
    let note = Note::parse(text.as_bytes()).unwrap();
    let edited = set_key(&note, "summary", &json!("Short")).unwrap();
    assert_eq!(
        edited.to_bytes(),
        b"---\nsummary: Short\n# keep\nnext: unchanged\n---\n"
    );
}

#[test]
fn crlf_unicode_quoted_key_and_bom() {
    let text = "\u{feff}---\r\n'한글 키': \"유지\"\r\nnext: [z, a]\r\n---\r\n내용";
    let note = Note::parse(text.as_bytes()).unwrap();
    let edited = set_key(&note, "한글 키", &json!("변경")).unwrap();
    assert_eq!(
        edited.frontmatter().unwrap().get("한글 키"),
        Some(json!("변경"))
    );
    let created = set_key(
        &Note::parse("\u{feff}본문".as_bytes()).unwrap(),
        "type",
        &json!("note"),
    )
    .unwrap();
    assert_eq!(
        created.to_bytes(),
        "\u{feff}---\ntype: note\n---\n본문".as_bytes()
    );
}

#[test]
fn new_block_and_empty_frontmatter() {
    for original in ["body", "---\n---\nbody", "---\n# comment\n---\nbody"] {
        let note = Note::parse(original.as_bytes()).unwrap();
        let edited = set_key(&note, "title", &json!("a: b # c")).unwrap();
        assert_eq!(edited.body(), "body");
        assert_eq!(
            edited.frontmatter().unwrap().get("title"),
            Some(json!("a: b # c"))
        );
    }
}

#[test]
fn malformed_frontmatter_is_reported_and_refused() {
    for text in [
        "---\ntitle: unterminated",
        "---\ntitle: [broken\n---\nBody",
        "---\ntitle: one\ntitle: two\n---\nBody",
        "---\n- sequence\n---\nBody",
        "---\nscalar\n---\nBody",
        "---\na: b\n...\nb: c\n---\nBody",
    ] {
        let note = Note::parse(text.as_bytes()).unwrap();
        assert!(!note.diagnostics().is_empty(), "{text}");
        assert!(
            set_key(&note, "title", &json!("changed")).is_err(),
            "{text}"
        );
        assert!(remove_key(&note, "title").is_err(), "{text}");
        assert_eq!(note.to_bytes(), text.as_bytes());
        assert!(note.body().starts_with("---"));
    }
}

#[test]
fn read_only_yaml_forms_are_preserved() {
    for (text, reason) in [
        ("---\n{title: Flow}\n---\nBody", ReadOnlyReason::FlowMapping),
        (
            "---\ntitle: &a Anchored\nother: *a\n---\nBody",
            ReadOnlyReason::AnchorsOrAliases,
        ),
        (
            "---\ntitle:\n\tbad: tab\n---\nBody",
            ReadOnlyReason::TabIndentation,
        ),
    ] {
        let note = Note::parse(text.as_bytes()).unwrap();
        assert!(
            note.frontmatter()
                .as_ref()
                .unwrap()
                .read_only_reasons()
                .contains(&reason),
            "{:?}",
            note.diagnostics()
        );
        assert!(set_key(&note, "new", &json!(true)).is_err());
        assert_eq!(note.to_bytes(), text.as_bytes());
    }
}

#[test]
fn invalid_utf8_is_a_typed_error() {
    assert!(matches!(Note::parse(&[0xff]), Err(ParseError::Utf8(_))));
}

#[test]
fn lifecycle_shapes_report_diagnostics_and_plain_status_is_ignored() {
    let plain = Note::parse(b"---\nstatus: active\n---\n").unwrap();
    assert_eq!(plain.lifecycle().lifecycle_type, LifecycleType::None);
    assert_eq!(plain.lifecycle().status, None);
    let note=Note::parse(b"---\ntype: memory\nstatus: active\nclaim_kind: current-state\nkind: fact\nexcluded: wrong\nsources: not-a-list\n---\n").unwrap();
    assert_eq!(note.lifecycle().status, Some(Status::Active));
    assert!(
        note.diagnostics()
            .iter()
            .any(|d| d.field.as_deref() == Some("excluded"))
    );
    assert!(
        note.diagnostics()
            .iter()
            .any(|d| d.field.as_deref() == Some("sources"))
    );
}

#[test]
fn typed_pins_and_lifecycle_preserve_unknown_metadata() {
    let text = "---\ntype: memory\nsources:\n  - ref: source.md\n    commit: null\n    pending: true\n    source_hash: sha256:abc\n    locator: {span: {start: 0, end: 12}}\napplicability: {from: null, until: null, conditions: [], supersedes: []}\nentities: []\norigin: {kind: agent, locator: 'mcp:wi_capture'}\nrestores: {ref: old.md, commit: abc}\ncustom: unchanged\n---\n";
    let note = Note::parse(text.as_bytes()).unwrap();
    assert!(note.diagnostics().is_empty(), "{:?}", note.diagnostics());
    let lifecycle = note.lifecycle();
    let pin = &lifecycle.sources.unwrap()[0];
    assert_eq!(pin.pending, Some(true));
    assert_eq!(pin.commit, None);
    assert_eq!(
        pin.locator,
        Some(Locator {
            location: Location::Span(Span { start: 0, end: 12 }),
            url: None
        })
    );
    assert_eq!(lifecycle.entities, Some(vec![]));
    assert_eq!(note.to_bytes(), text.as_bytes());
}

#[test]
fn config_preserves_unknown_keys_and_comments() {
    let text = "# vault\nlifecycle = 'on' # keep\ncustom = [3, 1]\n\n[unknown]\nanswer = 42\n";
    let config = VaultConfig::parse(text).unwrap();
    assert_eq!(config.to_text(), text);
    assert_eq!(config.stale_after_days(), 30);
    let changed = config
        .set("lifecycle", toml_edit::Value::from("off"))
        .unwrap();
    assert_eq!(changed.lifecycle(), "off");
    assert!(changed.to_text().contains("custom = [3, 1]"));
    assert!(changed.to_text().contains("[unknown]\nanswer = 42"));
    assert!(changed.to_text().contains("# keep"));
    assert!(
        config
            .set("lifecycle", toml_edit::Value::from("invalid"))
            .is_err()
    );
    assert!(VaultConfig::parse("vault_id = 'not-a-uuid'").is_err());
    assert!(VaultConfig::parse("vault_id = 'd9428888-122b-4e1e-b85c-61cd3cbb3210'").is_ok());
}

fn arbitrary_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        prop::collection::vec(any::<char>(), 0..60)
            .prop_map(|chars| Value::String(chars.into_iter().collect()))
    ]
    .prop_recursive(3, 48, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
            prop::collection::btree_map(".*", inner, 0..5)
                .prop_map(|m| Value::Object(m.into_iter().collect()))
        ]
    })
}
proptest! {
    #[test]
    fn property_body_round_trip(body in prop::collection::vec(any::<char>(),0..500)) {
        let body: String = body.into_iter().collect();
        let text=format!("Body\n{body}");
        let note = Note::parse(text.as_bytes()).unwrap();
        prop_assert_eq!(note.to_bytes(),text.as_bytes());
    }
    #[test]
    fn property_set_then_get(key in "[a-zA-Z_][a-zA-Z0-9_]{0,24}", value in arbitrary_value()) {
        let note=Note::parse(b"---\nunknown_list: [z, a]\n---\nBody\n").unwrap();
        let next=set_key(&note,&key,&value).unwrap();
        prop_assert_eq!(next.frontmatter().unwrap().get(&key),Some(value));
        prop_assert_eq!(next.body(),"Body\n");
    }
}

#[test]
fn scalar_encoding_handles_yaml_line_separators_and_numbers() {
    let note = Note::parse(b"body").unwrap();
    for value in [
        json!("line\u{85}next"),
        json!("line\u{2028}next"),
        json!("line\u{2029}next"),
        json!("many\nlines\r\n\0"),
        json!(18446744073709551615u64),
        json!(1.2e-30),
        json!("true"),
        json!("null"),
        json!("42"),
        json!("[list]"),
        json!({"": "value", "nested": [1,true,"x, y"]}),
    ] {
        let edited =
            set_key(&note, "test", &value).unwrap_or_else(|error| panic!("{value:?}: {error}"));
        assert_eq!(edited.frontmatter().unwrap().get("test"), Some(value));
    }
}

#[test]
fn leading_blank_and_whitespace_fences_preserved() {
    let text = "\n  \n--- \t\r\ntitle: Old\r\n---  \r\nBody";
    let note = Note::parse(text.as_bytes()).unwrap();
    assert_eq!(note.body(), "Body");
    let edited = set_key(&note, "title", &json!("New")).unwrap();
    assert_eq!(
        edited.to_bytes(),
        text.replace("title: Old", "title: New").as_bytes()
    );
}

#[test]
fn explicit_document_end_is_read_only() {
    let note = Note::parse(b"---\ntitle: Old\n...\n---\nBody").unwrap();
    assert!(set_key(&note, "new", &json!("value")).is_err());
}

#[test]
fn indented_adjacent_comments_and_literal_hash_content() {
    let text = "---\ntitle: Original\n  # keep this comment\nnext: unchanged\n---\nBody";
    let edited = set_key(
        &Note::parse(text.as_bytes()).unwrap(),
        "title",
        &json!("Changed"),
    )
    .unwrap();
    assert_eq!(
        edited.to_bytes(),
        text.replace("title: Original", "title: Changed").as_bytes()
    );
    let text = "---\nsummary: |\n  # literal content\n# keep comment\nnext: unchanged\n---\nBody";
    let edited = remove_key(&Note::parse(text.as_bytes()).unwrap(), "summary").unwrap();
    assert_eq!(
        edited.to_bytes(),
        b"---\n# keep comment\nnext: unchanged\n---\nBody"
    );
}
