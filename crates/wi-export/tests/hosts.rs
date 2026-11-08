use serde_json::{Value, json};
use tempfile::TempDir;
use wi_export::{
    hosts::{self, Format, HOSTS, HostId},
    install,
};
#[test]
fn every_installed_host_is_detected_registered_idempotently_and_removed() {
    if isolated("every_installed_host_is_detected_registered_idempotently_and_removed") {
        return;
    }
    let temp = TempDir::new().unwrap();
    let home = temp.path();
    assert!(hosts::detect(home).is_empty());
    let vault_path = home.join("vault");
    let vault = vault_path.as_path();
    for host in HOSTS {
        let (directory, path) = host.id.paths(home, None);
        std::fs::create_dir_all(directory).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let initial = match host.format {
            Format::Toml => {
                "# keep comment\nmodel = 'chosen'\n[mcp_servers.other]\ncommand = 'other'\n"
                    .to_owned()
            }
            Format::Json(key) => format!(
                "{{\n  \"theme\" : 1e+02, \"{key}\": {{\"other\":{{\"command\":\"other\"}}}}\n}}\n"
            ),
        };
        std::fs::write(&path, &initial).unwrap();
        let plan = install::plan_host(
            host.id,
            home,
            None,
            &std::env::temp_dir().join("wi-test-bin"),
            vault,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), initial);
        plan.apply().unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        match host.format {
            Format::Toml => {
                assert!(after.contains("# keep comment\nmodel = 'chosen'"));
            }
            Format::Json(key) => {
                assert!(after.contains("\"theme\" : 1e+02"));
                let value: Value = serde_json::from_str(&after).unwrap();
                let entry = &value[key]["wiki-island"];
                match host.id {
                    HostId::Zed => assert_eq!(
                        entry["command"],
                        json!({"path":std::env::temp_dir().join("wi-test-bin"),"args":["mcp","--vault",vault]})
                    ),
                    HostId::Opencode => assert_eq!(
                        entry["command"],
                        json!([
                            std::env::temp_dir().join("wi-test-bin"),
                            "mcp",
                            "--vault",
                            vault
                        ])
                    ),
                    _ => assert_eq!(entry["args"], json!(["mcp", "--vault", vault])),
                }
            }
        }
        assert!(
            !install::plan_host(
                host.id,
                home,
                None,
                &std::env::temp_dir().join("wi-test-bin"),
                vault
            )
            .unwrap()
            .changed
        );
        assert!(
            install::plan_remove(
                host.id,
                home,
                None,
                &std::env::temp_dir().join("another-vault")
            )
            .unwrap()
            .is_none()
        );
        install::plan_remove(host.id, home, None, vault)
            .unwrap()
            .unwrap()
            .apply()
            .unwrap();
        assert!(
            install::plan_remove(host.id, home, None, vault)
                .unwrap()
                .is_none()
        );
        let removed = std::fs::read_to_string(path).unwrap();
        assert!(removed.contains("other"));
        assert!(!removed.contains("wiki-island"));
    }
    assert_eq!(hosts::detect(home).len(), HOSTS.len());
}
#[test]
fn uninstall_retains_unrelated_json_bytes_for_first_middle_last_and_only_members() {
    if isolated("uninstall_retains_unrelated_json_bytes_for_first_middle_last_and_only_members") {
        return;
    }
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(".claude.json");
    let vault = temp.path().join("vault");
    let owned = format!(
        r#""wiki-island" : {{"command":{},"args":["mcp","--vault",{}]}}"#,
        json!(temp.path().join("wi.exe")),
        json!(&vault)
    );
    for members in [
        owned.to_owned(),
        format!("{owned}, \"keep\" : 1e02"),
        format!("\"keep\" : 1e02,{owned}"),
        format!("\"keep\" : 1e02,{owned},\"last\":[]"),
    ] {
        std::fs::write(
            &path,
            format!("{{\"mcpServers\":{{ {members} }},\"outside\": [1, 2]}}\n"),
        )
        .unwrap();
        install::plan_remove(HostId::Claude, temp.path(), None, &vault)
            .unwrap()
            .unwrap()
            .apply()
            .unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let _: Value = serde_json::from_str(&after).unwrap();
        assert!(after.contains("\"outside\": [1, 2]"));
        if members.contains("keep") {
            assert!(after.contains("\"keep\" : 1e02"));
        }
    }
}

#[test]
fn jsonc_comments_trailing_commas_and_existing_opencode_jsonc_survive() {
    if isolated("jsonc_comments_trailing_commas_and_existing_opencode_jsonc_survive") {
        return;
    }
    let temp = TempDir::new().unwrap();
    for host in [HostId::Opencode, HostId::Vscode, HostId::Zed] {
        let path = if host == HostId::Opencode {
            temp.path().join(".config/opencode/opencode.jsonc")
        } else {
            host.config_path(temp.path(), None)
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let Format::Json(key) = host.spec().format else {
            unreachable!()
        };
        for inside in [
            "",
            "\"other\" : {\"url\":\"https://example.invalid/a//b\"}, // retain this\n",
        ] {
            let before = format!(
                "{{\n // retain header\n \"font\": [1, 2,], /* retain setting */\n \"{key}\": {{ {inside} }},\n}}\n"
            );
            std::fs::write(&path, &before).unwrap();
            let plan = install::plan_host(
                host,
                temp.path(),
                None,
                &std::env::temp_dir().join("wi-test-bin"),
                &temp.path().join("vault"),
            )
            .unwrap();
            assert_eq!(plan.path, path);
            assert!(plan.warnings.is_empty());
            plan.apply().unwrap();
            let after = std::fs::read_to_string(&path).unwrap();
            assert!(after.contains("// retain header"));
            assert!(after.contains("\"font\": [1, 2,], /* retain setting */"));
            assert!(after.contains(inside));
            assert!(
                !install::plan_host(
                    host,
                    temp.path(),
                    None,
                    &std::env::temp_dir().join("wi-test-bin"),
                    &temp.path().join("vault")
                )
                .unwrap()
                .changed
            );
            install::plan_remove(host, temp.path(), None, &temp.path().join("vault"))
                .unwrap()
                .unwrap()
                .apply()
                .unwrap();
            let removed = std::fs::read_to_string(&path).unwrap();
            assert!(removed.contains("// retain header"));
            assert!(removed.contains("\"font\": [1, 2,], /* retain setting */"));
            if !inside.is_empty() {
                assert!(removed.contains("\"other\" : {\"url\":\"https://example.invalid/a//b\"}"));
                assert!(removed.contains("// retain this\n"));
            }
            assert!(!removed.contains("wiki-island"));
        }
    }
}

#[test]
fn gemini_cli_settings_are_registered_in_addition_to_the_ide() {
    if isolated("gemini_cli_settings_are_registered_in_addition_to_the_ide") {
        return;
    }
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join(".gemini")).unwrap();
    for host in hosts::detect(temp.path()) {
        install::plan_host(
            host,
            temp.path(),
            None,
            &std::env::temp_dir().join("wi-test-bin"),
            &temp.path().join("vault"),
        )
        .unwrap()
        .apply()
        .unwrap();
    }
    assert!(temp.path().join(".gemini/settings.json").is_file());
    assert!(temp.path().join(".gemini/config/mcp_config.json").is_file());
}

#[test]
fn uninstall_after_binary_upgrade_preserves_intervening_user_settings() {
    if isolated("uninstall_after_binary_upgrade_preserves_intervening_user_settings") {
        return;
    }
    let temp = TempDir::new().unwrap();
    let vault = temp.path().join("vault");
    for host in [HostId::Claude, HostId::Codex, HostId::Cursor] {
        let path = host.config_path(temp.path(), None);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = if host == HostId::Codex {
            "# keep\nmodel = 'first'\n"
        } else {
            "{\"theme\":\"first\"}\n"
        };
        std::fs::write(&path, original).unwrap();
        install::plan_host(
            host,
            temp.path(),
            None,
            &std::env::temp_dir().join("wi-test-old-bin"),
            &vault,
        )
        .unwrap()
        .apply()
        .unwrap();
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("first", "second");
        std::fs::write(&path, edited).unwrap();
        install::plan_host(
            host,
            temp.path(),
            None,
            &std::env::temp_dir().join("wi-test-new-bin"),
            &vault,
        )
        .unwrap()
        .apply()
        .unwrap();
        install::plan_remove(host, temp.path(), None, &vault)
            .unwrap()
            .unwrap()
            .apply()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original.replace("first", "second")
        );
    }
}

#[test]
fn codex_tables_allow_later_servers_and_convert_inline_settings() {
    if isolated("codex_tables_allow_later_servers_and_convert_inline_settings") {
        return;
    }
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(".codex/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    for before in [
        "",
        "model = 'chosen'\nmcp_servers = { keep = { command = 'keep' }, wiki-island = { command = 'old', args = [] } }\n",
    ] {
        std::fs::write(&path, before).unwrap();
        let plan = install::plan_host(
            HostId::Codex,
            temp.path(),
            None,
            &std::env::temp_dir().join("wi-test-bin"),
            &temp.path().join("vault"),
        )
        .unwrap();
        plan.apply().unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("[mcp_servers.wiki-island]"), "{after}");
        let appended = format!("{after}\n[mcp_servers.foo]\ncommand = 'foo'\n");
        let parsed = appended.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["foo"]["command"].as_str(),
            Some("foo")
        );
        if !before.is_empty() {
            assert_eq!(
                parsed["mcp_servers"]["keep"]["command"].as_str(),
                Some("keep")
            );
            assert_eq!(parsed["model"].as_str(), Some("chosen"));
        }
    }
}

// Process-local environment isolation also permits tests to run in parallel.
fn isolated(_name: &str) -> bool {
    #[cfg(windows)]
    if std::env::var("WI_HOST_TEST_CHILD").as_deref() != Ok(_name) {
        let temp = TempDir::new().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", _name, "--nocapture"])
            .env("WI_HOST_TEST_CHILD", _name)
            .env("HOME", temp.path())
            .env("USERPROFILE", temp.path())
            .env("APPDATA", temp.path().join("roaming"))
            .env("LOCALAPPDATA", temp.path().join("local"))
            .env_remove("CODEX_HOME")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return true;
    }
    false
}
