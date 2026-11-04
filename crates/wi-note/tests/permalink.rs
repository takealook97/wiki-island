use std::collections::HashSet;
use wi_note::*;

// Cover normalized paths, transliteration and extension handling.
#[test]
fn generated_permalink_matches_expected_slugs() {
    for (path, expected) in [
        ("docs/My Feature.md", "docs/my-feature"),
        ("specs/API (v2).md", "specs/api-v2"),
        (
            "design/unified_model_refactor.md",
            "design/unified-model-refactor",
        ),
        ("MIXED_CASE_NAME.md", "mixed-case-name"),
        ("Version 2.0.0", "version-2.0.0"),
        ("test/Über File.md", "test/uber-file"),
        ("docs/résumé.md", "docs/resume"),
        ("notes/Déjà vu.md", "notes/deja-vu"),
        ("papers/Jürgen's Findings.md", "papers/jurgens-findings"),
        ("archive/François Müller.md", "archive/francois-muller"),
        ("research/Søren Kierkegård.md", "research/soren-kierkegard"),
        ("ArticlesElNiño.md", "articles-el-nino"),
        ("中文/测试文档.md", "中文/测试文档"),
        ("docs/中文 English Mixed.md", "docs/中文-english-mixed"),
        ("articles/东京Tokyo混合.md", "articles/东京-tokyo-混合"),
        (
            "projects/中文CamelCase测试.md",
            "projects/中文-camel-case-测试",
        ),
        ("mixed/北京Café.md", "mixed/北京-cafe"),
        ("punctuation/你好，世界!.md", "punctuation/你好世界"),
        ("결제/환불 정책.md", "gyeolje/hwanbul-jeongcaeg"),
    ] {
        assert_eq!(generate_permalink(path), expected, "{path}");
    }
}

#[test]
fn unicode_normalization() {
    assert_eq!(
        generate_permalink("réglage.md"),
        generate_permalink("re\u{301}glage.md")
    );
    assert_eq!(generate_permalink("한글.md"), generate_permalink("한글.md"));
}
#[test]
fn generated_collision_uses_first_free_suffix() {
    let existing = HashSet::from([
        "docs/my-note".into(),
        "docs/my-note-1".into(),
        "docs/my-note-3".into(),
    ]);
    assert_eq!(
        resolve_permalink(None, "docs/My Note.md", &existing).unwrap(),
        "docs/my-note-2"
    );
}
#[test]
fn explicit_collision_is_typed_refusal() {
    let existing = HashSet::from(["owned".into()]);
    assert_eq!(
        resolve_permalink(Some("owned"), "other.md", &existing),
        Err(PermalinkError::ExplicitCollision("owned".into()))
    );
}
#[test]
fn explicit_precedence_and_move_identity() {
    let existing = HashSet::new();
    assert_eq!(
        resolve_permalink(Some("s/ses_AbCdEf"), "different.md", &existing).unwrap(),
        "s/ses_AbCdEf"
    );
    let retained = resolve_permalink(None, "before.md", &existing).unwrap();
    assert_eq!(
        resolve_permalink(Some(&retained), "after.md", &existing).unwrap(),
        retained
    );
    assert_eq!(
        resolve_permalink(None, "!!!.md", &existing),
        Err(PermalinkError::Empty)
    );
}
