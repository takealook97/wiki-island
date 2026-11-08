use serde_json::Value;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn install_cli_requires_yes_and_export_all_reports_caps() {
    let temp = TempDir::new().unwrap();
    let vault = temp.path().join("vault");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_wi"))
            .env("HOME", temp.path())
            .env("USERPROFILE", temp.path())
            .env("APPDATA", temp.path())
            .env("LOCALAPPDATA", temp.path())
            .env("WI_CACHE_DIR", temp.path().join("cache"))
            .env_remove("CODEX_HOME")
            .arg("--vault")
            .arg(&vault)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(run(&["init"]).status.success());
    for (host, path) in [
        ("claude", ".claude.json"),
        ("codex", ".codex/config.toml"),
        ("agy", ".gemini/config/mcp_config.json"),
    ] {
        let preview = run(&["mcp", "install", "--host", host, "--json"]);
        assert!(
            preview.status.success(),
            "{}",
            String::from_utf8_lossy(&preview.stderr)
        );
        let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
        assert_eq!(preview["applied"], false);
        assert!(
            preview["plan"]["diff"]
                .as_str()
                .unwrap()
                .contains("wiki-island")
        );
        assert!(!temp.path().join(path).exists());
        let apply = run(&["mcp", "install", "--host", host, "--yes", "--json"]);
        assert!(apply.status.success());
        let apply: Value = serde_json::from_slice(&apply.stdout).unwrap();
        assert_eq!(apply["applied"], true);
        assert!(temp.path().join(path).exists());
    }
    let export = run(&["export", "rules", "--host", "all", "--json"]);
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let export: Value = serde_json::from_slice(&export.stdout).unwrap();
    assert_eq!(export["exports"].as_array().unwrap().len(), 3);
    for report in export["exports"].as_array().unwrap() {
        assert!(report["bytes"].as_u64().unwrap() <= 32768);
        assert!(vault.join(report["path"].as_str().unwrap()).exists());
    }
}

#[test]
fn host_install_keeps_separate_settings_backups_and_idempotence() {
    for (host, config_rel, unrelated_rel) in [
        (
            "agy",
            ".gemini/config/mcp_config.json",
            ".gemini/settings.json",
        ),
        (
            "gemini",
            ".gemini/settings.json",
            ".gemini/config/mcp_config.json",
        ),
    ] {
        for initial in [
            None,
            Some(""),
            Some(
                r#"{"unknown":{"keep":true},"mcpServers":{"other":{"command":"other","env":{"KEEP":"yes"}},"wiki-island":{"disabled":true,"custom":"keep","env":{"TOKEN":"test"}}}}"#,
            ),
        ] {
            let home = TempDir::new().unwrap();
            let vault = home.path().join("vault");
            std::fs::create_dir(&vault).unwrap();
            let vault = std::fs::canonicalize(vault).unwrap();
            let config = home.path().join(config_rel);
            let legacy = home.path().join(unrelated_rel);
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            // Registering one host must not read or change the other host settings.
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, "legacy sentinel").unwrap();
            if let Some(bytes) = initial {
                std::fs::write(&config, bytes).unwrap();
            }
            let run = |host: &str, yes: bool| {
                let mut command = Command::new(env!("CARGO_BIN_EXE_wi"));
                command
                    .env("HOME", home.path())
                    .env("USERPROFILE", home.path())
                    .env("APPDATA", home.path())
                    .env("LOCALAPPDATA", home.path())
                    .env_remove("CODEX_HOME")
                    .args(["--vault"])
                    .arg(&vault)
                    .args(["mcp", "install", "--host", host, "--json"]);
                if yes {
                    command.arg("--yes");
                }
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                serde_json::from_slice::<Value>(&output.stdout).unwrap()
            };
            let preview = run(host, false);
            assert_eq!(preview["plan"]["path"], config.to_str().unwrap());
            assert_eq!(preview["applied"], false);
            assert!(preview["plan"]["diff"].as_str().unwrap().contains("+"));
            assert_eq!(
                std::fs::read(&config).ok(),
                initial.map(|s| s.as_bytes().to_vec())
            );
            let applied = run(host, true);
            assert_eq!(applied["applied"], true);
            if let Some(original) = initial {
                let backup = applied["backup"].as_str().unwrap();
                assert_eq!(std::fs::read(backup).unwrap(), original.as_bytes());
                assert!(
                    std::path::Path::new(backup)
                        .starts_with(vault.join(".wiki-island/local/backups"))
                );
            } else {
                assert!(applied["backup"].is_null());
            }
            let after = std::fs::read_to_string(&config).unwrap();
            let doc: Value = serde_json::from_str(&after).unwrap();
            if initial.is_none_or(str::is_empty) {
                assert!(after.starts_with("{\n  \"mcpServers\": {\n"));
            } else {
                assert!(after.starts_with(r#"{"unknown":{"keep":true},"mcpServers":{"other":{"command":"other","env":{"KEEP":"yes"}},"wiki-island":"#));
            }
            let entry = &doc["mcpServers"]["wiki-island"];
            if host == "agy" {
                assert_eq!(entry["disabled"], false);
            }
            assert_eq!(
                entry["command"],
                std::path::absolute(env!("CARGO_BIN_EXE_wi"))
                    .unwrap()
                    .to_str()
                    .unwrap()
            );
            assert_eq!(entry["args"], serde_json::json!(["mcp", "--vault", vault]));
            if initial.is_some_and(|s| !s.is_empty()) {
                assert_eq!(doc["unknown"]["keep"], true);
                assert_eq!(
                    doc["mcpServers"]["other"],
                    serde_json::json!({"command":"other","env":{"KEEP":"yes"}})
                );
                assert_eq!(entry["custom"], "keep");
                assert_eq!(entry["env"]["TOKEN"], "test");
            }
            let file_count = std::fs::read_dir(config.parent().unwrap()).unwrap().count();
            for _ in 0..2 {
                let next = run(host, true);
                assert_eq!(next["plan"]["changed"], false);
                assert_eq!(next["plan"]["diff"], "");
                assert!(next["backup"].is_null());
                assert_eq!(std::fs::read_to_string(&config).unwrap(), after);
            }
            assert_eq!(
                std::fs::read_dir(config.parent().unwrap()).unwrap().count(),
                file_count
            );
            assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "legacy sentinel");
        }
    }
}

#[test]
fn mcp_install_help_names_separate_ide_and_cli_hosts() {
    let output = Command::new(env!("CARGO_BIN_EXE_wi"))
        .args(["mcp", "install", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("agy"));
    assert!(help.contains("gemini"));
    assert!(help.contains("Gemini CLI"));
}
