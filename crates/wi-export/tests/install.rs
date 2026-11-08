use serde_json::{Value, json};
#[cfg(unix)]
use std::path::Path;
use tempfile::TempDir;
use wi_export::{Host, install};

#[test]
fn install_preview_backup_and_unrelated_config_survive_for_every_host() {
    for host in Host::ALL {
        let home = TempDir::new().unwrap();
        let custom = home.path().join("custom-codex");
        let path = match host {
            Host::Codex => custom.join("config.toml"),
            Host::Claude => home.path().join(".claude.json"),
            Host::Gemini => home.path().join(".gemini/settings.json"),
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let before = if host == Host::Codex {
            "# retain comment\nmodel = 'existing'\n[mcp_servers.other]\ncommand = 'other'\n[mcp_servers.wiki-island]\ncommand = 'old'\nurl = 'https://example.invalid/mcp'\nenabled = false\n"
        } else {
            r#"{"theme":"dark","mcpServers":{"other":{"command":"other"},"wiki-island":{"command":"old","url":"https://example.invalid/mcp","enabled":false}}}"#
        };
        std::fs::write(&path, before).unwrap();
        let plan = install::plan(
            host,
            home.path(),
            Some(&custom),
            &std::env::temp_dir().join("wi-test-bin"),
            &home.path().join("vault"),
        )
        .unwrap();
        assert!(plan.changed);
        assert!(plan.diff.contains("+"));
        assert_eq!(plan.path, path);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "preview mutated config"
        );
        let backup = plan.apply().unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(backup).unwrap(), before);
        let after = std::fs::read_to_string(&path).unwrap();
        if host == Host::Codex {
            assert!(after.starts_with("# retain comment"));
            let doc = after.parse::<toml_edit::DocumentMut>().unwrap();
            assert!(doc["mcp_servers"]["wiki-island"].get("url").is_none());
            assert_eq!(doc["model"].as_str(), Some("existing"));
            assert_eq!(
                doc["mcp_servers"]["other"]["command"].as_str(),
                Some("other")
            );
            assert_eq!(
                doc["mcp_servers"]["wiki-island"]["enabled"].as_bool(),
                Some(false)
            );
            assert_eq!(
                doc["mcp_servers"]["wiki-island"]["args"]
                    .as_array()
                    .unwrap()
                    .len(),
                3
            );
        } else {
            let doc: Value = serde_json::from_str(&after).unwrap();
            assert!(doc["mcpServers"]["wiki-island"].get("url").is_none());
            if host == Host::Claude {
                assert_eq!(doc["mcpServers"]["wiki-island"]["type"], "stdio");
            }
            assert_eq!(doc["theme"], "dark");
            assert_eq!(doc["mcpServers"]["other"]["command"], "other");
            assert_eq!(doc["mcpServers"]["wiki-island"]["enabled"], false);
            assert_eq!(
                doc["mcpServers"]["wiki-island"]["args"],
                json!(["mcp", "--vault", home.path().join("vault")])
            );
        }
        let next = install::plan(
            host,
            home.path(),
            Some(&custom),
            &std::env::temp_dir().join("wi-test-bin"),
            &home.path().join("vault"),
        )
        .unwrap();
        assert!(!next.changed);
        assert!(
            next.diff.is_empty(),
            "idempotent plan printed {:?}",
            next.diff
        );
        assert!(next.apply().unwrap().is_none());
        assert!(!home.path().join(".codex/config.toml").exists());
    }
}
#[test]
fn install_absent_configs_stale_preview_and_invalid_shape() {
    let home = TempDir::new().unwrap();
    for host in Host::ALL {
        let plan = install::plan(
            host,
            home.path(),
            None,
            &std::env::temp_dir().join("wi-test-bin"),
            &home.path().join("vault"),
        )
        .unwrap();
        assert!(!plan.path.exists());
        assert!(plan.apply().unwrap().is_none());
        assert!(plan.path.exists());
        let stale = install::plan(
            host,
            home.path(),
            None,
            &std::env::temp_dir().join("wi-test-new-bin"),
            &home.path().join("vault"),
        )
        .unwrap();
        std::fs::write(&stale.path, "modified").unwrap();
        assert!(matches!(stale.apply(), Err(install::Error::Stale)));
        std::fs::write(
            &stale.path,
            if host == Host::Codex {
                "mcp_servers = 42"
            } else {
                "{\"mcpServers\":42}"
            },
        )
        .unwrap();
        assert!(
            install::plan(
                host,
                home.path(),
                None,
                &std::env::temp_dir().join("wi-test-bin"),
                &home.path().join("vault")
            )
            .is_err()
        );
        if host == Host::Codex {
            std::fs::write(&stale.path, "[mcp_servers]\nwiki-island = 42\n").unwrap();
            assert!(
                install::plan(
                    host,
                    home.path(),
                    None,
                    &std::env::temp_dir().join("wi-test-bin"),
                    &home.path().join("vault")
                )
                .is_err()
            );
        }
    }
}
#[cfg(unix)]
#[test]
fn install_follows_a_symlink_config_and_writes_private_files() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let home = TempDir::new().unwrap();
    let outside = home.path().join("other.json");
    std::fs::write(&outside, "{}").unwrap();
    symlink(&outside, home.path().join(".claude.json")).unwrap();
    let linked = install::plan(
        Host::Claude,
        home.path(),
        None,
        &std::env::temp_dir().join("wi-test-bin"),
        &home.path().join("vault"),
    )
    .unwrap();
    linked.apply().unwrap();
    assert!(
        std::fs::symlink_metadata(home.path().join(".claude.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&outside).unwrap()).unwrap()["mcpServers"]["wiki-island"]
            ["command"],
        std::env::temp_dir().join("wi-test-bin").to_str().unwrap()
    );
    let plan = install::plan(
        Host::Gemini,
        home.path(),
        None,
        &std::env::temp_dir().join("wi-test-bin"),
        &home.path().join("vault"),
    )
    .unwrap();
    plan.apply().unwrap();
    assert_eq!(
        std::fs::metadata(plan.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn json_install_preserves_unrelated_bytes_key_order_and_escaped_strings() {
    let home = TempDir::new().unwrap();
    let path = home.path().join(".claude.json");
    let prefix = "{\n  \"z-last\" : [ {\"quoted\":\"brace } and escaped \\\"\"} ],\n  \"mcpServers\" : { \"other\": {\"args\":[1,2]} , \"wiki-island\" : ";
    let suffix = " },\n  \"a-first\": 1e+02\n}\n";
    let before = format!("{prefix}{{\"command\":\"old\",\"custom\":true}}{suffix}");
    serde_json::from_str::<Value>(&before).unwrap();
    std::fs::write(&path, &before).unwrap();
    let plan = install::plan(
        Host::Claude,
        home.path(),
        None,
        &std::env::temp_dir().join("wi-test-bin"),
        &home.path().join("vault"),
    )
    .unwrap();
    plan.apply().unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.starts_with(prefix), "{after}");
    assert!(after.ends_with(suffix), "{after}");
    assert!(plan.warnings.is_empty());
    assert!(
        serde_json::from_str::<Value>(&after).unwrap()["mcpServers"]["wiki-island"]["custom"]
            .as_bool()
            .unwrap()
    );
    assert!(!plan.diff.contains("-  \"z-last\""));
}

#[cfg(unix)]
#[test]
fn replacement_preserves_existing_config_mode() {
    use std::os::unix::fs::PermissionsExt;
    for host in Host::ALL {
        for mode in [0o600, 0o640] {
            let home = TempDir::new().unwrap();
            let initial = install::plan(
                host,
                home.path(),
                None,
                Path::new("/bin/old"),
                &home.path().join("vault"),
            )
            .unwrap();
            initial.apply().unwrap();
            std::fs::set_permissions(&initial.path, std::fs::Permissions::from_mode(mode)).unwrap();
            let update = install::plan(
                host,
                home.path(),
                None,
                Path::new("/bin/new"),
                &home.path().join("vault"),
            )
            .unwrap();
            update.apply().unwrap();
            assert_eq!(
                std::fs::metadata(&initial.path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                mode
            );
        }
    }
}

#[test]
fn json_insertion_preserves_existing_members_and_warns_on_duplicate_keys() {
    for before in [
        "{ \"z\" : 1 }\n",
        "{\n\t\"mcpServers\": {\"other\" : []}\n}\n",
    ] {
        let home = TempDir::new().unwrap();
        let path = home.path().join(".claude.json");
        std::fs::write(&path, before).unwrap();
        let plan = install::plan(
            Host::Claude,
            home.path(),
            None,
            &std::env::temp_dir().join("wi-test-bin"),
            &home.path().join("vault"),
        )
        .unwrap();
        plan.apply().unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains(if before.contains("mcpServers") {
            "\"other\" : []"
        } else {
            "\"z\" : 1"
        }));
        assert!(plan.warnings.is_empty());
        assert!(
            serde_json::from_str::<Value>(&after).unwrap()["mcpServers"]["wiki-island"].is_object()
        );
    }
    let home = TempDir::new().unwrap();
    std::fs::write(home.path().join(".claude.json"), "{\"z\":1,\"z\":2}").unwrap();
    let plan = install::plan(
        Host::Claude,
        home.path(),
        None,
        &std::env::temp_dir().join("wi-test-bin"),
        &home.path().join("vault"),
    )
    .unwrap();
    assert_eq!(plan.warnings.len(), 1);
}

#[cfg(windows)]
#[test]
fn install_follows_a_windows_config_symlink_without_replacing_it() {
    let home = TempDir::new().unwrap();
    let target = home.path().join("other.json");
    let link = home.path().join(".claude.json");
    std::fs::write(&target, "{}").unwrap();
    std::os::windows::fs::symlink_file(&target, &link).unwrap();
    let plan = install::plan(
        Host::Claude,
        home.path(),
        None,
        &home.path().join("wi.exe"),
        &home.path().join("vault"),
    )
    .unwrap();
    plan.apply().unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        std::fs::read_to_string(target)
            .unwrap()
            .contains("wiki-island")
    );
}
