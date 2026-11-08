//! `wi publish notion` surface without network or keychain access. Homes,
//! data and caches are temporary.
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Env {
    _temp: tempfile::TempDir,
    base: PathBuf,
}
impl Env {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().to_path_buf();
        let e = Self { _temp: temp, base };
        assert!(e.run(&["init"], "").status.success());
        e
    }
    fn run(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_wi"))
            .args(args)
            .arg("--json")
            .env("WI_VAULT", self.base.join("notes"))
            .env("HOME", self.base.join("home"))
            .env("USERPROFILE", self.base.join("home"))
            .env("APPDATA", self.base.join("home"))
            .env("LOCALAPPDATA", self.base.join("home"))
            .env("WI_DATA_DIR", self.base.join("data"))
            .env("WI_CACHE_DIR", self.base.join("cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

#[test]
fn status_and_unconfigured_publish_need_no_network_or_keychain() {
    let e = Env::new();
    let out = e.run(&["publish", "notion", "status"], "");
    assert!(out.status.success(), "{out:?}");
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["configured"], false);
    let out = e.run(&["publish", "notion"], "");
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("wi publish notion setup"), "{text}");
}

#[test]
fn setup_rejects_bad_parents_and_tokens_before_touching_the_keychain() {
    let e = Env::new();
    let out = e.run(
        &["publish", "notion", "setup", "--parent", "not-a-page"],
        "ntn_placeholder_token_value_0000\n",
    );
    assert!(!out.status.success());
    let out = e.run(
        &[
            "publish",
            "notion",
            "setup",
            "--parent",
            "11111111111111111111111111111111",
        ],
        "bad token with spaces\n",
    );
    assert!(!out.status.success());
    let both = [out.stdout, out.stderr].concat();
    assert!(!String::from_utf8_lossy(&both).contains("bad token"));
    assert!(
        !e.base.join("data/vaults").exists() || {
            let status: Value =
                serde_json::from_slice(&e.run(&["publish", "notion", "status"], "").stdout)
                    .unwrap();
            status["configured"] == false
        }
    );
}
