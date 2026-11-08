//! Publishing wiring against a loopback mock only: no real service, no
//! real keychain, temporary data directories.
use super::*;
use std::net::TcpListener;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU16, Ordering},
};

const PARENT: &str = "11111111-1111-1111-1111-111111111111";
const PAGE: &str = "22222222-2222-2222-2222-222222222222";
const TOKEN: &str = "ntn_test_placeholder_token_0123456789";

struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    status: Arc<AtomicU16>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Mock {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(AtomicU16::new(200));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, code, halt) = (requests.clone(), status.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            while !halt.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                let mut head = Vec::new();
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                assert!(head.contains(&format!("Bearer {TOKEN}")));
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|v| v.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let mut first = head.lines().next().unwrap().split_whitespace();
                let (method, path) = (first.next().unwrap(), first.next().unwrap());
                log.lock()
                    .unwrap()
                    .push((method.to_owned(), path.to_owned()));
                let status = code.load(Ordering::Relaxed);
                let reply = match method {
                    _ if status != 200 => json!({"object":"error"}),
                    "POST" => json!({"id": PAGE}),
                    "GET" => json!({"results": [], "has_more": false}),
                    "PATCH" if path.contains("/blocks/") => json!({"results":
                        body["children"].as_array().map_or(vec![], |c| c.iter().enumerate()
                            .map(|(i, _)| json!({"id": format!("33333333-3333-3333-3333-{i:012}")})).collect())}),
                    _ => json!({}),
                };
                let text = reply.to_string();
                write!(stream, "HTTP/1.1 {status} r\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{text}", text.len()).unwrap();
            }
        });
        Self {
            base,
            requests,
            status,
            stop,
            handle: Some(handle),
        }
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    fn posts(&self) -> usize {
        let log = self.requests.lock().unwrap();
        log.iter().filter(|(m, _)| m == "POST").count()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.take().unwrap().join();
    }
}

struct Token;
impl TokenSource for Token {
    fn token(&self) -> wi_notion::Result<String> {
        Ok(TOKEN.into())
    }
}

#[derive(Default)]
struct Memory(Mutex<Vec<(String, String)>>);
impl SecretStore for Memory {
    fn save(&self, account: &str, secret: &str) -> Result<()> {
        self.0.lock().unwrap().push((account.into(), secret.into()));
        Ok(())
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    vault: Vault,
}

fn placement(temp: &Path) -> (wi_store::OpenOptions, wi_store::Placement) {
    (
        wi_store::OpenOptions {
            index_path: Some(temp.join("index.sqlite3")),
        },
        wi_store::Placement {
            storage: Some(wi_store::StorageMode::Git),
            data_root: Some(temp.join("data")),
        },
    )
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let (options, place) = placement(temp.path());
    let (vault, _) = Vault::init_placed(temp.path().join("notes"), options, place).unwrap();
    assert!(
        vault
            .device_state_dir()
            .unwrap()
            .starts_with(temp.path().join("data"))
    );
    Fixture { temp, vault }
}

fn save(vault: &Vault, path: &str, text: &str) {
    std::fs::write(vault.root().join(path), text).unwrap();
    vault.ingest_path(path).unwrap();
    vault.flush().unwrap();
}

/// Every file under `dir`, recursively, as bytes.
fn all_bytes(dir: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.extend(bytes);
            }
        }
    }
    out
}

#[test]
fn parent_accepts_page_links_and_ids() {
    for input in [
        "https://www.notion.so/team/Wiki-11111111111111111111111111111111?pvs=4",
        "https://notion.so/11111111111111111111111111111111",
        "11111111-1111-1111-1111-111111111111",
    ] {
        assert_eq!(parse_parent(input).unwrap(), PARENT, "{input}");
    }
    for input in [
        "",
        "https://notion.so/page",
        "zz111111111111111111111111111111",
    ] {
        assert!(parse_parent(input).is_err(), "{input}");
    }
}

#[test]
fn setup_keeps_the_token_out_of_the_vault_and_data_directory() {
    let f = fixture();
    let secrets = Memory::default();
    assert!(setup_with(&f.vault, PARENT, "short", &secrets).is_err());
    assert!(secrets.0.lock().unwrap().is_empty());
    setup_with(&f.vault, PARENT, TOKEN, &secrets).unwrap();
    assert_eq!(
        *secrets.0.lock().unwrap(),
        [(f.vault.vault_id().to_owned(), TOKEN.to_owned())]
    );
    let haystack = all_bytes(f.temp.path());
    assert!(
        !haystack.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()),
        "token was written to disk"
    );
    let value = status(&f.vault).unwrap();
    assert_eq!(value["configured"], true);
    assert_eq!(value["parent_page_id"], PARENT);
    assert!(!value.to_string().contains(TOKEN));
}

#[test]
fn unconfigured_device_neither_publishes_nor_starts_a_thread() {
    let f = fixture();
    assert!(publish_now(&f.vault, &Token, None, false).is_err());
    assert!(start(&f.vault).unwrap().is_none());
    assert_eq!(status(&f.vault).unwrap()["configured"], false);
}

#[test]
fn publishes_only_committed_notes_and_is_idempotent() {
    let f = fixture();
    let mock = Mock::new();
    setup_with(&f.vault, PARENT, TOKEN, &Memory::default()).unwrap();
    save(&f.vault, "a.md", "# A\n\nhello\n");
    // Uncommitted working-tree bytes are never published.
    std::fs::write(f.vault.root().join("draft.md"), "# Draft\n").unwrap();
    let first = publish_now(&f.vault, &Token, Some(&mock.base), false).unwrap();
    assert_eq!(first["created"], json!(["a"]), "{first}");
    assert_eq!(first["failed"], json!([]));
    assert_eq!(mock.posts(), 1);
    let before = mock.count();
    let again = publish_now(&f.vault, &Token, Some(&mock.base), false).unwrap();
    assert_eq!(again["unchanged"], 1);
    assert_eq!(mock.count(), before, "unchanged notes make no requests");
    save(&f.vault, "a.md", "# A\n\nchanged\n");
    let changed = publish_now(&f.vault, &Token, Some(&mock.base), false).unwrap();
    assert_eq!(changed["updated"], json!(["a"]));
    std::fs::remove_file(f.vault.root().join("draft.md")).unwrap();
    std::fs::remove_file(f.vault.root().join("a.md")).unwrap();
    f.vault.auto_ingest().unwrap();
    f.vault.flush().unwrap();
    let gone = publish_now(&f.vault, &Token, Some(&mock.base), false).unwrap();
    assert_eq!(gone["archived"], json!(["a"]));
    assert_eq!(status(&f.vault).unwrap()["published"], 0);
    // The map is per device: it lives in the data directory, not the vault.
    assert!(
        f.vault
            .device_state_dir()
            .unwrap()
            .join("notion/map.json")
            .is_file()
    );
    assert!(
        !all_bytes(f.vault.root())
            .windows(PAGE.len())
            .any(|w| w == PAGE.as_bytes())
    );
}

#[test]
fn failures_back_off_and_background_attempts_wait() {
    let f = fixture();
    let mock = Mock::new();
    setup_with(&f.vault, PARENT, TOKEN, &Memory::default()).unwrap();
    save(&f.vault, "a.md", "# A\n");
    mock.status.store(401, Ordering::Relaxed);
    let failed = publish_now(&f.vault, &Token, Some(&mock.base), false).unwrap();
    assert_eq!(failed["failed"].as_array().unwrap().len(), 1, "{failed}");
    let value = status(&f.vault).unwrap();
    assert_eq!(value["failures"], 1);
    assert!(value["next_attempt_ms"].as_u64().unwrap() > now_ms() + 30_000);
    let requests = mock.count();
    let deferred = publish_now(&f.vault, &Token, Some(&mock.base), true).unwrap();
    assert_eq!(deferred["deferred"], true);
    assert_eq!(mock.count(), requests);
    // An explicit command ignores the backoff and resets it on success.
    mock.status.store(200, Ordering::Relaxed);
    let ok = publish_now(&f.vault, &Token, Some(&mock.base), false).unwrap();
    assert_eq!(ok["failed"], json!([]));
    assert_eq!(status(&f.vault).unwrap()["failures"], 0);
}

#[test]
fn background_thread_publishes_after_the_tip_settles_and_stops() {
    let f = fixture();
    let mock = Mock::new();
    setup_with(&f.vault, PARENT, TOKEN, &Memory::default()).unwrap();
    save(&f.vault, "a.md", "# A\n");
    let root = f.vault.root().to_path_buf();
    let base = f.temp.path().to_path_buf();
    let timing = Timing {
        settle: Duration::from_millis(200),
        period: Duration::from_secs(300),
        tick: Duration::from_millis(20),
    };
    let handle = start_with(
        &f.vault,
        timing,
        Some(mock.base.clone()),
        Box::new(Token),
        move || {
            let (options, place) = placement(&base);
            Vault::open_placed(root, options, place).ok()
        },
    )
    .unwrap()
    .expect("configured device starts a publisher");
    let deadline = Instant::now() + Duration::from_secs(20);
    while mock.posts() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    handle.stop();
    assert_eq!(mock.posts(), 1);
}

#[test]
fn a_parent_with_multibyte_characters_is_refused_without_panicking() {
    assert!(parse_parent(&format!("위키{}", "1".repeat(30))).is_err());
    assert!(parse_parent("페이지-é").is_err());
    let id = "0123456789abcdef0123456789abcdef";
    assert_eq!(
        parse_parent(&format!("https://www.notion.so/위키-{id}")).unwrap(),
        "01234567-89ab-cdef-0123-456789abcdef"
    );
}

#[test]
fn setup_cannot_switch_the_parent_while_a_first_publish_is_running() {
    let f = fixture();
    let mock = Mock::new();
    setup_with(&f.vault, PARENT, TOKEN, &Memory::default()).unwrap();
    save(&f.vault, "a.md", "# A\n");
    struct Switch<'a>(&'a Vault, std::cell::Cell<bool>);
    impl TokenSource for Switch<'_> {
        fn token(&self) -> wi_notion::Result<String> {
            let other = "33333333-3333-3333-3333-333333333333";
            let refused = setup_with(self.0, other, TOKEN, &Memory::default()).is_err();
            self.1.set(refused);
            Ok(TOKEN.to_owned())
        }
    }
    let switch = Switch(&f.vault, std::cell::Cell::new(false));
    let first = publish_now(&f.vault, &switch, Some(&mock.base), false).unwrap();
    assert_eq!(first["failed"], json!([]));
    assert!(switch.1.get(), "setup changed the parent during a publish");
    assert_eq!(configured(&f.vault).unwrap().as_deref(), Some(PARENT));
    save(&f.vault, "a.md", "# A changed\n");
    let retry = publish_now(&f.vault, &Token, Some(&mock.base), false);
    assert!(retry.is_ok(), "{retry:?}");
}

#[test]
fn the_token_prompt_never_reads_when_echo_cannot_be_turned_off() {
    let mut input = std::io::Cursor::new(format!("{TOKEN}\n"));
    assert!(read_hidden(|| false, &mut input).is_err());
    assert_eq!(input.position(), 0, "the token was read while echo was on");
    let mut input = std::io::Cursor::new(format!("{TOKEN}\n"));
    assert_eq!(read_hidden(|| true, &mut input).unwrap(), TOKEN);
}
