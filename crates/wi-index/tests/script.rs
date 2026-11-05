//! Unicode normalization, script boundaries and query analysis scenarios.
use wi_index::script::*;

#[test]
fn script_runs_cover_scripts_and_nfkc() {
    for (text, expected) in [
        ("适者生存", vec![vec!["适", "者", "生", "存"]]),
        ("適者生存", vec![vec!["適", "者", "生", "存"]]),
        ("サバイバル", vec![vec!["サ", "バ", "イ", "バ", "ル"]]),
        ("생존 경쟁", vec![vec!["생", "존"], vec!["경", "쟁"]]),
        ("ภาษาไทย", vec![vec!["ภ", "า", "ษ", "า", "ไ", "ท", "ย"]]),
        ("時々", vec![vec!["時", "々"]]),
        ("ＡＢＣ", vec![]),
    ] {
        assert_eq!(script_runs(text), expected, "{text}");
    }
}
#[test]
fn script_grams_preserve_order_single_characters_and_boundaries() {
    assert_eq!(
        script_run_grams(&["适".into(), "者".into(), "生".into(), "存".into()]),
        ["适者", "者生", "生存"]
    );
    assert_eq!(script_run_grams(&["猫".into()]), ["猫"]);
    assert_eq!(
        build_script_ngrams(&["适者", "生存"]),
        "适 者 适者 script_boundary 生 存 生存"
    );
}
#[test]
fn script_combining_marks_and_join_controls_attach_to_units() {
    assert_eq!(script_runs("漢\u{fe00}"), vec![vec!["漢\u{fe00}"]]);
    for join in ["\u{200c}", "\u{200d}"] {
        assert_eq!(script_runs(&format!("ក{join}ខ")), vec![vec!["ក", "ខ"]]);
    }
}
#[test]
fn script_query_separates_words_and_ordered_script_terms() {
    let query = analyze_script_query("OpenAI 适者生存，サバイバル");
    assert_eq!(query.word_text.as_deref(), Some("OpenAI"));
    assert_eq!(
        query.gram_phrases,
        vec![
            vec!["适者", "者生", "生存"],
            vec!["サバ", "バイ", "イバ", "バル"]
        ]
    );
    for text in [
        "foo適者bar",
        "foo-適者-bar",
        "foo適者",
        "ＡＢＣ適者",
        "適者OpenAI",
    ] {
        let query = analyze_script_query(text);
        assert_eq!(query.word_text.as_deref(), Some(text));
        assert_eq!(query.gram_phrases, vec![vec!["適者"]]);
    }
}
#[test]
fn script_boolean_quotes_punctuation_and_compatibility_forms() {
    for text in [
        "OpenAI OR 适者生存",
        "NOT 生存",
        "生存 OR",
        "\"OpenAI 适者生存\"",
        "!!!",
        "😀",
    ] {
        let query = analyze_script_query(text);
        assert_eq!(query.word_text.as_deref(), Some(text));
        assert!(query.gram_phrases.is_empty());
    }
    assert_eq!(
        analyze_script_query("OpenAI and 适者生存")
            .word_text
            .as_deref(),
        Some("OpenAI and")
    );
    assert_eq!(
        analyze_script_query("适者 ＡＮＤ 生存")
            .word_text
            .as_deref(),
        Some("ＡＮＤ")
    );
    assert_eq!(
        analyze_script_query("适者\tAND\t生存").word_text.as_deref(),
        Some("and")
    );
}
