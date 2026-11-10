//! The CLI reports refusals as plain sentences with stable JSON codes.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Env {
    _dir: TempDir,
    cache: TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        let env = Self {
            _dir: dir,
            cache,
            root,
        };
        let out = env.wi(&["init", env.root.to_str().unwrap()]);
        assert!(out.status.success(), "{out:?}");
        env
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wi"));
        cmd.args(args)
            .env("WI_CACHE_DIR", self.cache.path())
            .env("WI_VAULT", &self.root)
            .env("WI_SYSTEMCTL", "/bin/false")
            .env("WI_SCHTASKS", "wi-missing-schtasks.exe")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("WI_FAULT")
            .env_remove("WI_DEBUG");
        cmd
    }

    fn wi(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn set_limit(&self, limit: usize) {
        let config = self.root.join(".wiki-island/config.toml");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(&format!("max_note_bytes = {limit}\n"));
        std::fs::write(&config, text).unwrap();
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn json_error(out: &Output) -> Value {
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    body["error"].clone()
}

#[test]
fn wi_read_refuses_a_file_above_the_read_limit_with_a_plain_sentence() {
    let env = Env::new();
    env.set_limit(32);
    std::fs::write(env.root.join("big.md"), "x".repeat(33)).unwrap();
    std::fs::write(env.root.join("ok.md"), "y".repeat(32)).unwrap();

    let out = env.wi(&["read", "big.md"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let text = stderr(&out);
    assert!(
        text.contains("big.md is larger than the read limit"),
        "{text}"
    );
    assert!(text.contains("max_note_bytes"), "{text}");
    assert!(!text.contains(env.root.to_str().unwrap()), "{text}");
    assert!(!text.contains("xxxx"), "{text}");

    let out = env.wi(&["--json", "read", "big.md"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(json_error(&out)["code"], "too-large");

    let out = env.wi(&["read", "ok.md"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "y".repeat(32));
}

#[test]
fn wi_search_refuses_an_over_cap_query_as_an_invalid_request() {
    let env = Env::new();
    let out = env
        .command(&["write", "a.md"])
        .arg("--content")
        .arg("alpha beta\n")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    for query in [
        vec!["alpha"; 65].join(" "),
        "a".repeat(2049),
        "a".repeat(16_385),
    ] {
        let out = env.wi(&["--json", "search", &query]);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let error = json_error(&out);
        assert_eq!(error["code"], "invalid-request");
        assert_eq!(
            error["message"],
            "The search text is too long. Search for a few key words instead."
        );
        let text = stderr(&out);
        assert!(text.contains("The search text is too long."), "{text}");
        assert!(!text.contains("recovering"), "{text}");
    }
    let out = env.wi(&["--json", "search", &vec!["alpha"; 64].join(" ")]);
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn wi_debug_adds_the_original_detail_on_its_own_line() {
    let env = Env::new();
    let plain = env.wi(&["read", "missing.md"]);
    assert_eq!(plain.status.code(), Some(1), "{plain:?}");
    let text = stderr(&plain);
    assert!(text.contains("error: missing.md does not exist."), "{text}");
    assert!(!text.contains("detail:"), "{text}");

    let debug = env
        .command(&["read", "missing.md"])
        .env("WI_DEBUG", "1")
        .output()
        .unwrap();
    assert_eq!(debug.status.code(), plain.status.code());
    let text = stderr(&debug);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("error: missing.md does not exist.")),
        "{text}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("detail: ") && l.contains("does not exist")),
        "{text}"
    );

    // JSON output gains no field.
    let json = env
        .command(&["--json", "read", "missing.md"])
        .env("WI_DEBUG", "1")
        .output()
        .unwrap();
    let body: Value = serde_json::from_slice(&json.stdout).unwrap();
    let mut keys: Vec<&str> = body["error"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["code", "hint", "message"]);
    assert_eq!(body["error"]["code"], "not-found");
}

#[test]
fn a_broken_settings_file_error_names_the_file_and_hides_absolute_paths() {
    let env = Env::new();
    let home = env._dir.path().join("home");
    let codex = env._dir.path().join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&codex).unwrap();
    let hidden = "/private/review-hidden/secret.md";
    std::fs::write(
        codex.join("config.toml"),
        format!("path=\"{hidden}\" garbage\n"),
    )
    .unwrap();
    let out = env
        .command(&["--json", "mcp", "install", "--host", "codex"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CODEX_HOME", &codex)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let error = json_error(&out);
    assert_eq!(error["code"], "install-config");
    let message = error["message"].as_str().unwrap();
    let text = stderr(&out);
    for shown in [message, text.as_str()] {
        assert!(!shown.contains(hidden), "{shown}");
        assert!(!shown.contains("review-hidden"), "{shown}");
        assert!(!shown.contains("garbage"), "{shown}");
        assert!(!shown.trim().contains('\n'), "{shown}");
    }
    assert!(message.contains("config.toml"), "{message}");
    assert!(
        message.contains("wi doctor") || message.contains("Fix"),
        "{message}"
    );
}

#[cfg(unix)]
#[test]
fn a_failed_service_command_is_reported_as_a_service_problem() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new();
    let script = env._dir.path().join("failing-service-tool");
    // The tool answers the systemd user-manager probe, so Linux treats the manager as present
    // and calls it to stop the unit; every other call, on any platform, fails with raw text.
    std::fs::write(
        &script,
        "#!/bin/sh\nfor arg in \"$@\"; do [ \"$arg\" = show-environment ] && exit 0; done\n\
         echo 'Operation not permitted' >&2\necho 'Try running again.' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = env
        .command(&["--json", "service", "uninstall"])
        .env("WI_LAUNCHCTL", &script)
        .env("WI_SYSTEMCTL", &script)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let error = json_error(&out);
    assert_eq!(error["code"], "service", "{error}");
    let message = error["message"].as_str().unwrap();
    let text = stderr(&out);
    for shown in [message, text.as_str()] {
        assert!(!shown.contains("settings file"), "{shown}");
        assert!(!shown.contains("Operation not permitted"), "{shown}");
        assert!(
            !shown.contains(env._dir.path().to_str().unwrap()),
            "{shown}"
        );
    }
    assert!(message.contains("background"), "{message}");
    assert!(message.contains("Run wi doctor"), "{message}");
}
