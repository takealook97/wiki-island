//! Observation, relation and link syntax scenarios.
use wi_note::*;

fn note(text: &str) -> Note {
    Note::parse(text.as_bytes()).unwrap()
}
fn relation(source: &str, typ: &str, target: &str, context: Option<&str>) {
    let relations = note(source).relations();
    assert_eq!(relations.len(), 1, "{source}: {relations:?}");
    assert_eq!(relations[0].rel_type, typ, "{source}");
    assert_eq!(relations[0].target, target, "{source}");
    assert_eq!(relations[0].context.as_deref(), context, "{source}");
}

#[test]
fn empty_input() {
    for source in ["", "   ", "\n"] {
        assert!(note(source).observations().is_empty());
        assert!(note(source).relations().is_empty());
    }
}
#[test]
fn observation_context() {
    let obs = note("- [test] Content (unclosed").observations();
    assert_eq!(obs[0].content, "Content (unclosed");
    assert_eq!(obs[0].context, None);
    let obs = note("- [test] Content (with) extra) parens)").observations();
    assert_eq!(obs[0].content, "Content");
    assert_eq!(obs[0].context.as_deref(), Some("with) extra) parens"));
}
#[test]
fn complex_category_and_compound_hashtags() {
    let obs = note("- [complex test] This is #tag1#tag2 with #tag3 content").observations();
    assert_eq!(obs[0].category.as_deref(), Some("complex test"));
    assert_eq!(obs[0].tags, vec!["tag1", "tag2", "tag3"]);
    assert_eq!(obs[0].content, "This is #tag1#tag2 with #tag3 content");
}
#[test]
fn unicode_observations() {
    let obs = note(
        "- [中文] Chinese text 测试 #language (Script test)\n- [test] Mixed 中文 and 👍 #mixed",
    )
    .observations();
    assert_eq!(obs.len(), 2);
    assert_eq!(obs[0].category.as_deref(), Some("中文"));
    assert_eq!(obs[0].context.as_deref(), Some("Script test"));
    assert_eq!(obs[1].content, "Mixed 中文 and 👍 #mixed");
}
#[test]
fn timestamp_categories_are_excluded() {
    for prefix in [
        "1:02",
        "00:00:11",
        "1:02:03.500",
        "100:02:11",
        "12:03,250",
        "24:33.098 - 24:41.260",
        "1:02 - 2:03",
    ] {
        assert!(
            note(&format!("- [{prefix}] speech"))
                .observations()
                .is_empty(),
            "{prefix}"
        );
    }
}
#[test]
fn numeric_non_timestamp_categories_remain() {
    for category in [
        "2024",
        "v1:2",
        "10:30am",
        "10:30am - 11:30am",
        "a",
        "1",
        "q",
    ] {
        assert_eq!(
            note(&format!("- [{category}] content")).observations()[0]
                .category
                .as_deref(),
            Some(category)
        );
    }
}
#[test]
fn hashtag_promoted_timecode_keeps_prefix() {
    let obs = note("- [00:00:11] Speaker: decision recorded #meeting").observations();
    assert_eq!(obs[0].category, None);
    assert_eq!(
        obs[0].content,
        "[00:00:11] Speaker: decision recorded #meeting"
    );
    assert_eq!(obs[0].tags, vec!["meeting"]);
}
#[test]
fn task_markers_are_excluded() {
    for marker in [" ", "x", "X", "-", "/", ">", "?", "!"] {
        assert!(
            note(&format!("- [{marker}] task"))
                .observations()
                .is_empty(),
            "{marker}"
        );
    }
}
#[test]
fn tag_only_observation_and_non_observations() {
    let obs = note("- This is #tag (context)").observations();
    assert_eq!(obs[0].category, None);
    assert_eq!(obs[0].tags, vec!["tag"]);
    for source in [
        "- [link](https://example.com)",
        "- [[Target]]",
        "- ordinary list item",
        "> [!info] Callout",
    ] {
        assert!(note(source).observations().is_empty(), "{source}");
    }
}
#[test]
fn empty_and_malformed_wikilinks() {
    for source in [
        "- type [[]]",
        "- type [[ ]]",
        "- type [[   ]]",
        "- type [[Target",
        "- type Target]]",
        "- type ]]Target[[",
    ] {
        assert!(note(source).relations().is_empty(), "{source}");
    }
}
#[test]
fn nested_wikilinks_depth_match() {
    relation(
        "- type [[Outer [[Inner]] ]]",
        "links_to",
        "Outer [[Inner]]",
        None,
    );
}
#[test]
fn explicit_relation_contexts() {
    relation("- type [[Target]] (unclosed", "links_to", "Target", None);
    relation(
        "- type [[Target]] (with (nested) parens)",
        "type",
        "Target",
        Some("with (nested) parens"),
    );
    relation("- type [[Target]] ()", "type", "Target", None);
}
#[test]
fn multiple_inline_relations() {
    let relations = note("Text with [[Link1]] and [[Link2]] and [[Link3]]").relations();
    assert_eq!(
        relations
            .iter()
            .map(|r| r.target.as_str())
            .collect::<Vec<_>>(),
        vec!["Link1", "Link2", "Link3"]
    );
    assert!(relations.iter().all(|r| r.rel_type == "links_to"));
}
#[test]
fn inline_code_exclusion_and_escapes() {
    relation(
        "The template is `[[Inline Target]]`; prose [[Real Target]].",
        "links_to",
        "Real Target",
        None,
    );
    assert!(
        note("``literal ` and [[Double Backtick Target]]``")
            .relations()
            .is_empty()
    );
    assert!(note(r"\[[Literal]]").relations().is_empty());
    relation(r"\`see [[Target]]\`", "links_to", "Target", None);
    relation(
        "- implemented_by [[Parser]] (`parse()`)",
        "implemented_by",
        "Parser",
        Some("`parse()`"),
    );
    relation(
        "[[Outer `[[literal]]` Target]]",
        "links_to",
        "Outer `[[literal]]` Target",
        None,
    );
}
#[test]
fn prose_tail_falls_back_without_losing_links() {
    for source in [
        "- Added [[Target Note]] to the roster",
        "- Calls [[Target Note]] every Sunday",
        "- \"multi word type\" [[Target Note]] trailing prose",
        "- type [[Target Note]] (context) and more",
    ] {
        relation(source, "links_to", "Target Note", None);
    }
    let rels = note("- relates_to [[Alpha]] (primary) and [[Beta]] (secondary)").relations();
    assert_eq!(
        rels.iter().map(|r| r.target.as_str()).collect::<Vec<_>>(),
        vec!["Alpha", "Beta"]
    );
    assert!(rels.iter().all(|r| r.rel_type == "links_to"));
}
#[test]
fn quoted_and_unquoted_relation_types() {
    relation(
        "- spouse_of [[Target Note]]",
        "spouse_of",
        "Target Note",
        None,
    );
    relation(
        "- requires [[Target Note]] (because reasons)",
        "requires",
        "Target Note",
        Some("because reasons"),
    );
    relation(
        "- \"multi word type\" [[Target Note]] (context)",
        "multi word type",
        "Target Note",
        Some("context"),
    );
    relation("- 'some type' [[Target]]", "some type", "Target", None);
    relation("- some other thing [[Target]]", "links_to", "Target", None);
    relation("- [[Target]]", "links_to", "Target", None);
    relation("- Mother [[Target Note]]", "Mother", "Target Note", None);
}
#[test]
fn unicode_relations() {
    relation("- type [[测试]]", "type", "测试", None);
    relation("- 使用 [[Target]] (测试)", "使用", "Target", Some("测试"));
    relation("- 관련 [[대상]] (한국어)", "관련", "대상", Some("한국어"));
}
#[test]
fn links_to_directive() {
    relation("- Mother [[Alice]] #bm:links_to", "links_to", "Alice", None);
    let parsed = note("- [note] Mother [[Alice]] #bm:links_to\n");
    assert_eq!(parsed.observations()[0].content, "Mother [[Alice]]");
    assert!(parsed.observations()[0].tags.is_empty());
    assert_eq!(parsed.relations()[0].rel_type, "links_to");
    assert!(
        note("- Mother [[Alice]] #bm:links_to")
            .observations()
            .is_empty()
    );
}
#[test]
fn fenced_and_indented_code_are_excluded() {
    let text = "```md\n- [fact] fake\n- calls [[Fake]]\n```\n\n~~~\n[[Fake2]]\n~~~\n\n    [[Indented]]\n\n- [fact] real\n- calls [[Real]]\n";
    let parsed = note(text);
    assert_eq!(parsed.observations().len(), 1);
    assert_eq!(parsed.relations().len(), 1);
    assert_eq!(parsed.relations()[0].target, "Real");
    assert_eq!(parsed.wikilinks().len(), 1);
}
#[test]
fn nested_list_and_blockquote() {
    let parsed = note(
        "- [test] Level 1 #test\n  - [test] Level 2 #test\n    - uses [[Two]]\n  - needs [[Three]]\n\n> [!info] Callout\n> [[Quoted Link]]\n",
    );
    assert_eq!(parsed.observations().len(), 2);
    assert_eq!(
        parsed
            .relations()
            .iter()
            .map(|r| r.rel_type.as_str())
            .collect::<Vec<_>>(),
        vec!["uses", "needs", "links_to"]
    );
}
#[test]
fn source_byte_and_line_ranges() {
    let parsed = note("---\ntitle: 한국어\n---\n\n- [fact] 내용\n\nSee [[대상|별칭]].\n");
    let links = parsed.wikilinks();
    assert_eq!(links[0].target, "대상");
    assert_eq!(links[0].alias.as_deref(), Some("별칭"));
    assert_eq!(
        &parsed.to_bytes()[links[0].byte_range.clone()],
        "[[대상|별칭]]".as_bytes()
    );
    assert_eq!(parsed.observations()[0].line_range, 5..6);
    assert_eq!(parsed.relations()[0].line_range, 7..8);
}
#[test]
fn semantic_opt_out_and_legacy_sidecars() {
    for key in ["parse_semantics", LEGACY_PARSE_SEMANTICS_KEY] {
        for value in ["false", "'FALSE'"] {
            let source =
                format!("---\n{key}: {value}\n---\n- [fact] extracted\n- relates_to [[Forged]]\n");
            let parsed = note(&source);
            assert!(parsed.observations().is_empty(), "{key}: {value}");
            assert!(parsed.relations().is_empty(), "{key}: {value}");
            assert!(parsed.wikilinks().is_empty(), "{key}: {value}");
            assert_eq!(parsed.to_bytes(), source.as_bytes());
        }
    }
    // The current key takes precedence when both spellings are present.
    let parsed = note(&format!(
        "---\nparse_semantics: true\n{LEGACY_PARSE_SEMANTICS_KEY}: false\n---\n- [fact] authored\n- relates_to [[Target]]\n"
    ));
    assert_eq!(parsed.observations().len(), 1);
    assert_eq!(parsed.relations().len(), 1);
    assert_eq!(parsed.wikilinks().len(), 1);
}
#[test]
fn project_namespace_and_markdown_file_links() {
    relation(
        "See [[project::/Target]]",
        "links_to",
        "project/Target",
        None,
    );
    relation(
        "[Doc](../docs/a%20b.md#heading)",
        "links_to",
        "../docs/a b.md",
        None,
    );
    relation("[Doc](note.md)", "links_to", "./note.md", None);
    assert!(
        note("[Web](https://example.com/a.md) [Fragment](#anchor)")
            .relations()
            .is_empty()
    );
}
#[test]
fn authored_samples_have_expected_items() {
    for (file, obs_count, targets) in [
        (
            "lighthouse.md",
            10,
            vec![
                "North Cove",
                "Tide Ledger",
                "Lens Manual",
                "Harbor Workshop",
                "Beacon Fleet",
            ],
        ),
        (
            "ceramics.md",
            7,
            vec!["East Arcade", "Kiln Two", "Stoneware Cups"],
        ),
        ("garden.md", 3, vec!["Seed Calendar"]),
        (
            "expedition.md",
            11,
            vec!["Autumn Map Campaign", "Valley Debrief"],
        ),
        (
            "catalog.md",
            7,
            vec![
                "Archive Crew",
                "Grid Reference",
                "Shelf Register",
                "Sheet Scanner",
                "Map Printer",
            ],
        ),
        (
            "explicit.md",
            7,
            vec!["Supply Shed", "Channel Plan", "Survey Crew"],
        ),
    ] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/notes/valid")
            .join(file);
        let parsed = Note::parse(&std::fs::read(path).unwrap()).unwrap();
        assert!(
            parsed.diagnostics().is_empty(),
            "{file}: {:?}",
            parsed.diagnostics()
        );
        assert_eq!(parsed.observations().len(), obs_count, "{file}");
        assert_eq!(
            parsed
                .relations()
                .iter()
                .map(|r| r.target.as_str())
                .collect::<Vec<_>>(),
            targets,
            "{file}"
        );
        assert_eq!(parsed.wikilinks().len(), targets.len(), "{file}");
    }
}
