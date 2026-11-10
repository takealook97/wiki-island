use std::process::Command;

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../wi-note/tests/fixtures/notes/valid/lighthouse.md")
}
#[test]
fn cli_json_parse() {
    let result = Command::new(env!("CARGO_BIN_EXE_wi"))
        .args(["note", "parse"])
        .arg(fixture())
        .arg("--json")
        .output()
        .unwrap();
    assert!(result.status.success(), "{:?}", result);
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["lifecycle_type"], "none");
    assert_eq!(json["observations"].as_array().unwrap().len(), 10);
    assert_eq!(json["relations"].as_array().unwrap().len(), 5);
    assert_eq!(json["wikilinks"].as_array().unwrap().len(), 5);
    assert!(json["diagnostics"].as_array().unwrap().is_empty());
}
#[test]
fn cli_human_parse() {
    let result = Command::new(env!("CARGO_BIN_EXE_wi"))
        .args(["note", "parse"])
        .arg(fixture())
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(
        String::from_utf8(result.stdout)
            .unwrap()
            .contains("Frontmatter keys: title, type, tags")
    );
}
#[test]
fn cli_missing_file_fails() {
    let result = Command::new(env!("CARGO_BIN_EXE_wi"))
        .args(["note", "parse"])
        .arg(fixture().with_file_name("deliberately-missing.md"))
        .arg("--json")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8(result.stderr).unwrap().contains("read"));
}
#[test]
fn both_cli_names_report_version() {
    for binary in [env!("CARGO_BIN_EXE_wi"), env!("CARGO_BIN_EXE_wiki-island")] {
        let result = Command::new(binary).arg("--version").output().unwrap();
        assert!(result.status.success());
        assert!(
            String::from_utf8(result.stdout)
                .unwrap()
                .contains(env!("CARGO_PKG_VERSION"))
        );
    }
}
