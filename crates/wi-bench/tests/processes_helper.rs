//! The process helper must read tool results in the shape the server sends
//! (docs/PROTOCOL.md): one TextContent block holding the envelope, no
//! `structuredContent`, and `isError: true` on errors.
#![cfg(not(windows))]

use std::process::Command;

const HELPER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/processes.py");

/// Run `tool_result` from processes.py on one wire result; return its
/// stdout on success or stderr on failure.
fn tool_result(wire: &serde_json::Value) -> Result<String, String> {
    let script = format!(
        "import importlib.util, json, sys\n\
         spec = importlib.util.spec_from_file_location('processes', {HELPER:?})\n\
         module = importlib.util.module_from_spec(spec)\n\
         spec.loader.exec_module(module)\n\
         print(json.dumps(module.tool_result(json.loads(sys.argv[1])), sort_keys=True))\n"
    );
    let output = Command::new("python3")
        .args(["-I", "-c", &script, &wire.to_string()])
        .output()
        .expect("python3 runs");
    if output.status.success() {
        Ok(String::from_utf8(output.stdout).unwrap().trim().to_owned())
    } else {
        Err(String::from_utf8(output.stderr).unwrap())
    }
}

#[test]
fn a_successful_result_is_read_from_its_text_block() {
    let envelope = serde_json::json!({"hits":[{"path":"source.md"}],"index":"ok"});
    let wire = serde_json::json!({
        "content": [{"type": "text", "text": envelope.to_string()}],
        "isError": false,
    });
    let value: serde_json::Value = serde_json::from_str(&tool_result(&wire).unwrap()).unwrap();
    assert_eq!(value, envelope);
}

#[test]
fn an_error_result_fails_the_measurement() {
    let wire = serde_json::json!({
        "content": [{"type": "text", "text": "{\"error\":{\"code\":\"not-found\"}}"}],
        "isError": true,
    });
    let error = tool_result(&wire).unwrap_err();
    assert!(error.contains("AssertionError"), "{error}");
}

#[test]
fn a_result_without_exactly_one_text_block_fails_the_measurement() {
    for wire in [
        serde_json::json!({"content": [], "isError": false}),
        serde_json::json!({"content": [{"type":"text","text":"{}"},{"type":"text","text":"{}"}]}),
        serde_json::json!({"content": [{"type":"image","data":"","mimeType":"image/png"}]}),
    ] {
        let error = tool_result(&wire).unwrap_err();
        assert!(error.contains("AssertionError"), "{wire}: {error}");
    }
}
