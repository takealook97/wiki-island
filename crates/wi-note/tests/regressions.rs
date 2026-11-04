//! Frontmatter encoding, parsing and permalink regressions.
use proptest::prelude::*;
use wi_note::*;

fn note(s: &str) -> Note {
    Note::parse(s.as_bytes()).unwrap()
}

const AMBIGUOUS: &[&str] = &[
    "yes",
    "no",
    "on",
    "off",
    "Yes",
    "NO",
    "y",
    "N",
    "true",
    "False",
    "NULL",
    "Null",
    "null",
    "~",
    "2024-01-01",
    "2024-01-01 10:00:00",
    "2001-12-14t21:59:43.10-05:00",
    "1_000",
    "0b101",
    "1:30",
    "12:30",
    "1:30:00",
    "190:20:30",
    "=",
    "<<",
    "tab\there",
    "0",
    ".nan",
    ".inf",
    "-1",
    "+1",
];

#[test]
fn yaml11_ambiguous_values_and_keys_are_quoted() {
    let original = note("---\ntitle: A\n---\nbody\n");
    for s in AMBIGUOUS {
        let value = Value::String((*s).into());
        let edited = set_key(&original, s, &value).unwrap();
        let bytes = std::str::from_utf8(edited.to_bytes()).unwrap();
        assert!(
            bytes.contains(&format!(
                "{}: {}\n",
                serde_json::to_string(s).unwrap(),
                serde_json::to_string(s).unwrap()
            )),
            "{s}: {bytes}"
        );
        assert_eq!(edited.frontmatter().unwrap().get(s), Some(value));
    }
    for s in ["ordinary text", "한국어", "letters/with-dots_1"] {
        let edited = set_key(&original, "key", &Value::String(s.into())).unwrap();
        assert!(
            std::str::from_utf8(edited.to_bytes())
                .unwrap()
                .contains(&format!("key: {s}\n"))
        );
    }
}

#[test]
#[ignore = "requires uv and PyYAML; run explicitly to check the YAML 1.1 reader"]
fn pyyaml_reads_written_keys_and_values_as_strings() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut cases = Vec::new();
    for s in AMBIGUOUS {
        let edited = set_key(&note("---\n---\n"), s, &Value::String((*s).into())).unwrap();
        cases.push(serde_json::json!([
            s,
            std::str::from_utf8(
                &edited.to_bytes()[edited.frontmatter().unwrap().content_range.clone()]
            )
            .unwrap()
        ]));
    }
    let mut child = Command::new("uv").args(["run", "--with", "pyyaml", "python", "-c", "import json,sys,yaml\nfor expected,text in json.load(sys.stdin):\n actual=yaml.safe_load(text)\n assert actual=={expected:expected}, (expected,actual)\n assert all(isinstance(k,str) and isinstance(v,str) for k,v in actual.items())\nprint('all YAML 1.1 keys and values remain strings')"]).stdin(Stdio::piped()).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&cases).unwrap().as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn transliteration_overrides_and_unmapped_characters() {
    for (input, expected) in [
        ("しごと.md", "shigoto"),
        ("つくば/チーム.md", "tsukuba/chi-mu"),
        ("ノート 日本.md", "no-to-日本"),
        ("ラーメン", "ra-men"),
        ("Єдність.md", "iednist"),
        ("a؀b.md", "ab"),
        ("aっb.md", "atsub"),
        ("aッb.md", "atsub"),
        ("aしちつシチツb.md", "ashichitsushichitsub"),
        ("a・b.md", "a-b"),
        ("a¼b.md", "a-1/4b"),
        ("a½b.md", "a-1/2b"),
        ("a¾b.md", "a-3/4b"),
        ("aŊb.md", "a-ngb"),
        ("aŋb.md", "angb"),
    ] {
        assert_eq!(generate_permalink(input), expected, "{input}");
    }
}

#[test]
fn relations_follow_source_order_within_and_between_blocks() {
    let parsed = note(
        "- [cat] [[Before]] and [md](./x.md) then [[After]]\n- depends_on [[A]] (needs A)\n\n- [cat] obs with [md](./y.md) link\n",
    );
    assert_eq!(
        parsed
            .relations()
            .iter()
            .map(|r| r.target.as_str())
            .collect::<Vec<_>>(),
        ["Before", "./x.md", "After", "A", "./y.md"]
    );
}

#[test]
fn removal_refuses_neighbour_value_changes() {
    for source in [
        "---\na: |+\n  keep\n\nb: 1\n\nc: 2\n---\n",
        "---\na: |\n  keep\nb: 1\n  # belongs outside a\nc: 2\n---\n",
    ] {
        let original = note(source);
        let result = remove_key(&original, "b");
        assert!(
            matches!(result, Err(EditError::WouldChangeOtherKey(ref key)) if key == "a"),
            "{result:?}"
        );
        assert_eq!(original.to_bytes(), source.as_bytes());
    }
}

proptest! {
    #[test]
    fn removal_preserves_all_other_values_or_refuses(
        keep in any::<bool>(), blanks_before in 0usize..5, blanks_after in 0usize..5,
        comment_indent in 0usize..5, comment in any::<bool>(), text in "[a-zA-Z ]{1,50}"
    ) {
        let source = format!("---\na: |{}\n  {}\n{}b: 1\n{}{}c: 2\n---\nbody\n", if keep { "+" } else { "" }, text, "\n".repeat(blanks_before), "\n".repeat(blanks_after), if comment { format!("{}# adjacent\n", " ".repeat(comment_indent)) } else { String::new() });
        let original = note(&source);
        if let Ok(edited) = remove_key(&original, "b") {
            for key in ["a", "c"] {
                prop_assert_eq!(edited.frontmatter().unwrap().get(key), original.frontmatter().unwrap().get(key));
            }
            prop_assert_eq!(edited.body(), original.body());
        }
    }
}

#[test]
fn new_keys_follow_frontmatter_fence_newline() {
    for (source, expected) in [
        ("---\ntitle: A\n---\nbody\r\n", "key: value\n---\nbody\r\n"),
        (
            "---\r\ntitle: A\r\n---\r\nbody\n",
            "key: value\r\n---\r\nbody\n",
        ),
        ("---\rtitle: A\r---\rbody\n", "key: value\r---\rbody\n"),
    ] {
        let edited = set_key(&note(source), "key", &Value::String("value".into())).unwrap();
        assert!(
            std::str::from_utf8(edited.to_bytes())
                .unwrap()
                .contains(expected)
        );
    }
}

#[test]
fn observation_edge_classes_and_escape_skip() {
    let parsed = note("- [tip] press # then enter");
    assert_eq!(parsed.observations()[0].tags, [""]);
    let parsed = note("- []   empty cat #t\nFootnote[^1]");
    assert_eq!(parsed.observations()[0].content, "empty cat #t");
    let parsed = note("[[a]] (ctx)\nText with #tag\nmore [[Real]]");
    assert_eq!(parsed.observations().len(), 1);
    let parsed = note("[a](./a.md)\nText #tag [b](./b.md)");
    assert_eq!(parsed.observations().len(), 1);
    let parsed = note(r"**> \[[[[B|c]]");
    assert_eq!(parsed.relations()[0].target, "B|c");
}

#[test]
fn body_trim_retains_source_offsets_and_cr_fences() {
    let source = "---\rtitle: A\r---\r    - child [[Deep]]\r";
    let parsed = note(source);
    assert_eq!(
        parsed.frontmatter().unwrap().get("title"),
        Some(Value::String("A".into()))
    );
    assert_eq!(parsed.relations()[0].rel_type, "child");
    assert_eq!(parsed.relations()[0].line_range, 4..5);
    assert_eq!(
        &source[parsed.wikilinks()[0].byte_range.clone()],
        "[[Deep]]"
    );
    assert_eq!(parsed.to_bytes(), source.as_bytes());
}

#[test]
fn python312_extension_and_path_edges() {
    for ext in [
        "yaml", "yml", "toml", "ini", "cfg", "jsx", "dat", "map", "php", "lua", "hpp", "cs", "vb",
        "pem", "img", "rpm", "war",
    ] {
        let file = format!("foo.{ext}");
        assert_eq!(generate_permalink(&file), file);
    }
    for ext in ["rst", "a", "o", "pyc"] {
        assert_eq!(generate_permalink(&format!("foo.{ext}")), "foo");
    }
    for (input, expected) in [
        ("....md", "....md"),
        (".md", ".md"),
        ("//double/x.md", "//double/x"),
        ("///double/x.md", "/double/x"),
        ("", "."),
    ] {
        assert_eq!(generate_permalink(input), expected, "{input}");
    }
}

#[test]
fn retired_directory_keys_still_parse_and_only_name_the_legacy_location() {
    // Directories are fixed now; old keys never fail a config and an unsafe
    // value falls back to the default legacy location.
    for path in [
        "../../etc",
        "/abs",
        "",
        "a/../../b",
        "a\\b",
        ".",
        "C:/abs",
        ".hidden",
    ] {
        let source = format!("captures_dir = {}", serde_json::to_string(path).unwrap());
        let config = VaultConfig::parse(&source).unwrap();
        assert_eq!(config.legacy_captures_dir(), "captures", "{path}");
        assert_eq!(config.captures_dir(), ".wiki-island/captures");
    }
    let config = VaultConfig::parse(
        "# retained\nunknown = 1\ncaptures_dir = 'inbox/notes'\nwork_dir = './tasks/'\narchive_dir = 'x'\n",
    )
    .unwrap();
    assert_eq!(
        config.to_text(),
        "# retained\nunknown = 1\ncaptures_dir = 'inbox/notes'\nwork_dir = './tasks/'\narchive_dir = 'x'\n"
    );
    assert_eq!(config.legacy_captures_dir(), "inbox/notes");
    assert_eq!(config.legacy_work_dir(), "tasks");
    assert_eq!(config.work_dir(), ".wiki-island/work");
    assert_eq!(config.archive_dir(), "x");
    for path in ["../up", "/abs", ".hidden", "a/../b", "", "C:/abs", "a\\b"] {
        let source = format!("archive_dir = {}", serde_json::to_string(path).unwrap());
        let config = VaultConfig::parse(&source).unwrap();
        assert_eq!(config.archive_dir(), "archive", "{path}");
    }
    let config = VaultConfig::parse("archive_dir = 'old/retired/'\n").unwrap();
    assert_eq!(config.archive_dir(), "old/retired");
    assert!(
        config
            .set("lifecycle", toml_edit::Value::from("sometimes"))
            .is_err()
    );
}

#[test]
fn locator_and_condition_diagnostics() {
    for locator in [
        "{span: {start: 1, end: 2}, page: {n: 1}}",
        "{span: {start: 5, end: 2}}",
    ] {
        let parsed = note(&format!(
            "---\ntype: memory\nsources: [{{ref: a.md, source_hash: hash, locator: {locator}}}]\n---\n"
        ));
        assert!(
            parsed
                .diagnostics()
                .iter()
                .any(|d| d.field.as_deref() == Some("sources"))
        );
        assert!(parsed.lifecycle().sources.is_none());
    }
    for (op, values) in [("eq", "[]"), ("eq", "[1, 2]"), ("in", "[]")] {
        let parsed = note(&format!(
            "---\ntype: memory\napplicability: {{conditions: [{{key: x, op: {op}, values: {values}}}]}}\n---\n"
        ));
        assert!(
            parsed
                .diagnostics()
                .iter()
                .any(|d| d.field.as_deref() == Some("applicability"))
        );
        assert!(parsed.lifecycle().applicability.is_none());
    }
}

#[test]
fn format_fields_are_exposed_without_defaulting_nulls() {
    let parsed = note(
        "---\ntype: work\ntags: []\nexclusion_reason: example\nskill_id: stable\ncapture_status: resolved\noutputs: [target]\nno_memory_reason: none\nwork: {target: target, state: ready, next_action: inspect, next_trigger: later, outcome: null, reason: null, limits: [], reopen: null}\n---\n",
    );
    let lc = parsed.lifecycle();
    assert!(
        parsed.diagnostics().is_empty(),
        "{:?}",
        parsed.diagnostics()
    );
    assert_eq!(lc.tags, Some(vec![]));
    assert_eq!(lc.exclusion_reason.as_deref(), Some("example"));
    assert_eq!(lc.skill_id.as_deref(), Some("stable"));
    assert_eq!(lc.capture_status, Some(CaptureStatus::Resolved));
    assert_eq!(lc.outputs, Some(vec!["target".into()]));
    assert_eq!(lc.no_memory_reason.as_deref(), Some("none"));
    assert_eq!(lc.work.unwrap().state, Some(WorkState::Ready));
    assert!(
        note("---\ntype: memory\ntags: null\n---\n")
            .lifecycle()
            .tags
            .is_none()
    );
}

#[test]
fn invalid_edited_yaml_reports_encoding() {
    let original = note("---\ntitle: A\n---\n");
    assert!(matches!(
        set_key(&original, &"a".repeat(1100), &Value::String("v".into())),
        Err(EditError::Encoding)
    ));
}

#[test]
#[ignore = "timing regression; run cargo test -p wi-note --release --test regressions two_megabytes_and_linear_scaling -- --ignored --nocapture"]
fn two_megabytes_and_linear_scaling() {
    use std::time::{Duration, Instant};
    fn measure(n: usize) -> Duration {
        let source = format!(
            "---\ntitle: large\n---\n{}",
            "- [fact] observation #tag `[[code]]`\n- uses [[Target]]\n".repeat(n)
        );
        let start = Instant::now();
        let parsed = note(&source);
        assert_eq!(parsed.observations().len(), n);
        assert_eq!(parsed.relations().len(), n);
        assert_eq!(parsed.wikilinks().len(), n);
        let elapsed = start.elapsed();
        eprintln!("bytes={} entries={n} accessors={elapsed:?}", source.len());
        elapsed
    }
    measure(100);
    let small = (0..3).map(|_| measure(10_000)).min().unwrap();
    let large = (0..3).map(|_| measure(40_000)).min().unwrap();
    assert!(
        large <= small * 6 + Duration::from_millis(20),
        "4x input: {small:?} -> {large:?}"
    );
    if !cfg!(debug_assertions) {
        assert!(large < Duration::from_secs(1), "{large:?}");
    }
}

#[test]
fn note_size_config_default_validation_and_unknown_keys() {
    let config = VaultConfig::parse("# keep\ncustom = 'untouched'\n").unwrap();
    assert_eq!(config.max_note_bytes(), 8 * 1024 * 1024);
    assert_eq!(config.to_text(), "# keep\ncustom = 'untouched'\n");
    assert_eq!(
        VaultConfig::parse("max_note_bytes = 1024")
            .unwrap()
            .max_note_bytes(),
        1024
    );
    for text in [
        "max_note_bytes = 0",
        "max_note_bytes = -1",
        "max_note_bytes = 'large'",
        "max_note_bytes = 1.5",
    ] {
        assert!(matches!(
            VaultConfig::parse(text),
            Err(ConfigError::Field(_))
        ));
    }
}
