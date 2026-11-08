use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use wi_notion::{MapStore, NoteSource, PublishNote, PublishOptions, TokenSource, publish};

const PARENT: &str = "11111111-1111-1111-1111-111111111111";
const PAGE: &str = "22222222-2222-2222-2222-222222222222";
const BLOCK: &str = "33333333-3333-3333-3333-333333333333";

#[derive(Clone, Debug)]
struct Request {
    method: String,
    path: String,
    body: Value,
}
struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Mock {
    fn new(mut respond: impl FnMut(&Request, usize) -> (u16, Value) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let halt = stop.clone();
        let handle = thread::spawn(move || {
            while !halt.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut byte = [0];
                while !bytes.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap_or_else(|e| {
                        panic!("header read failed after {} bytes: {e}", bytes.len())
                    });
                    bytes.push(byte[0]);
                }
                let headers = String::from_utf8(bytes).unwrap();
                assert!(headers.contains("notion-version: 2026-03-11\r\n"));
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|v| v.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                let mut first = headers.lines().next().unwrap().split_whitespace();
                let request = Request {
                    method: first.next().unwrap().into(),
                    path: first.next().unwrap().into(),
                    body: if body.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&body).unwrap()
                    },
                };
                let mut log = recorded.lock().unwrap();
                let (status, value) = respond(&request, log.len());
                log.push(request);
                drop(log);
                let body = value.to_string();
                write!(stream, "HTTP/1.1 {status} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            base,
            requests,
            stop,
            handle: Some(handle),
        }
    }
    fn options(&self) -> PublishOptions {
        let mut options = PublishOptions::new(PARENT);
        options.api_base_url = self.base.clone();
        options
    }
    fn log(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.handle.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}
fn success(request: &Request) -> (u16, Value) {
    let result = match request.method.as_str() {
        "POST" => json!({"id":PAGE}),
        "GET" => json!({"results":[], "has_more":false}),
        "PATCH" if request.path.contains("/blocks/") => {
            json!({"results":request.body["children"].as_array().unwrap().iter().map(|_| json!({"id":BLOCK})).collect::<Vec<_>>()})
        }
        _ => json!({}),
    };
    (200, result)
}
struct Source(Vec<PublishNote>);
impl NoteSource for Source {
    fn notes(&self) -> wi_notion::Result<Vec<PublishNote>> {
        Ok(self.0.clone())
    }
}
struct Token;
impl TokenSource for Token {
    fn token(&self) -> wi_notion::Result<String> {
        Ok("local-test-placeholder".into())
    }
}
struct NoToken;
impl TokenSource for NoToken {
    fn token(&self) -> wi_notion::Result<String> {
        panic!("should not read a credential")
    }
}
fn source(body: &str) -> Source {
    Source(vec![PublishNote {
        rel_path: "note.md".into(),
        bytes: body.as_bytes().to_vec(),
        permalink: "note".into(),
        title: "Note".into(),
        archived: false,
    }])
}

#[test]
fn creates_updates_skips_unchanged_and_archives_deleted_notes() {
    let mock = Mock::new(|request, _| {
        if request.method == "GET" {
            (200, json!({"results":[{"id":BLOCK}],"has_more":false}))
        } else {
            success(request)
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("map.json");
    let mut map = MapStore::open(&path).unwrap();
    let options = mock.options();
    let first = publish(&source("hello"), &Token, &mut map, &options).unwrap();
    assert_eq!(first.created, ["note"]);
    assert!(first.failed.is_empty());
    let log = mock.log();
    assert_eq!(
        log.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(),
        ["POST", "PATCH", "GET", "DELETE", "PATCH"]
    );
    let before = log.len();
    let mut map = MapStore::open(&path).unwrap();
    assert_eq!(
        publish(&source("hello"), &NoToken, &mut map, &options)
            .unwrap()
            .unchanged,
        ["note"]
    );
    assert_eq!(mock.log().len(), before);
    assert_eq!(
        publish(&source("changed"), &Token, &mut map, &options)
            .unwrap()
            .updated,
        ["note"]
    );
    assert_eq!(
        publish(&Source(vec![]), &Token, &mut map, &options)
            .unwrap()
            .archived,
        ["note"]
    );
    assert_eq!(mock.log().last().unwrap().body["in_trash"], true);
    assert!(MapStore::open(&path).unwrap().get("note").unwrap().archived);
    assert_eq!(
        publish(&source("changed"), &Token, &mut map, &options)
            .unwrap()
            .updated,
        ["note"]
    );
    assert!(!map.get("note").unwrap().archived);
}

#[test]
fn retries_throttles_without_duplicate_create_and_backs_off_repeatable_errors() {
    let mock = Mock::new(|r, index| {
        if index == 0 {
            (429, json!({}))
        } else if index == 3 {
            (503, json!({}))
        } else {
            success(r)
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let mut map = MapStore::open(dir.path().join("map")).unwrap();
    let report = publish(&source("hi"), &Token, &mut map, &mock.options()).unwrap();
    assert_eq!(report.created, ["note"]);
    assert!(report.failed.is_empty());
    assert_eq!(
        mock.log()
            .iter()
            .map(|r| r.method.as_str())
            .collect::<Vec<_>>(),
        ["POST", "POST", "PATCH", "GET", "GET", "PATCH"]
    );
}

#[test]
fn partial_append_failure_resumes_with_existing_page_and_replaces_children() {
    let mock = Mock::new(|r, index| {
        if index == 3 {
            (503, json!({"secret":"not included in errors"}))
        } else {
            success(r)
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("map");
    let mut map = MapStore::open(&path).unwrap();
    let options = mock.options();
    let failed = publish(&source("hi"), &Token, &mut map, &options).unwrap();
    assert_eq!(failed.failed.len(), 1);
    assert!(!failed.failed[0].reason.contains("secret"));
    assert!(map.get("note").unwrap().content_hash.is_empty());
    assert_eq!(mock.log().len(), 4, "ambiguous appends must not be retried");
    let mut map = MapStore::open(&path).unwrap();
    let report = publish(&source("hi"), &Token, &mut map, &options).unwrap();
    assert_eq!(report.updated, ["note"]);
    assert_eq!(mock.log().iter().filter(|r| r.method == "POST").count(), 1);
    assert!(!map.get("note").unwrap().content_hash.is_empty());
}

#[test]
fn dry_run_and_invalid_snapshot_never_write_or_read_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("map");
    let mut map = MapStore::open(&path).unwrap();
    let mut options = PublishOptions::new(PARENT);
    options.dry_run = true;
    assert_eq!(
        publish(&source("hello"), &NoToken, &mut map, &options)
            .unwrap()
            .created,
        ["note"]
    );
    assert!(!path.exists());
    let mut duplicate = source("hello");
    duplicate.0.push(duplicate.0[0].clone());
    assert!(publish(&duplicate, &NoToken, &mut map, &options).is_err());
    options.rate_limit = f64::MIN_POSITIVE;
    assert!(publish(&source("hello"), &NoToken, &mut map, &options).is_err());
    options.rate_limit = 3.0;
    options.api_base_url = "https://example.invalid/v1".into();
    assert!(publish(&source("hello"), &NoToken, &mut map, &options).is_err());
}

#[test]
fn paginates_children_and_appends_at_most_one_hundred_blocks() {
    let mock = Mock::new(|r, _| {
        if r.method == "GET" {
            if r.path.contains("start_cursor=") {
                assert!(r.path.contains("start_cursor=next%2Fpage"));
                return (200, json!({"results":[], "has_more":false}));
            }
            return (
                200,
                json!({"results":[{"id":BLOCK}],"has_more":true,"next_cursor":"next/page"}),
            );
        }
        success(r)
    });
    let dir = tempfile::tempdir().unwrap();
    let mut map = MapStore::open(dir.path().join("map")).unwrap();
    let report = publish(
        &source(&"paragraph\n\n".repeat(205)),
        &Token,
        &mut map,
        &mock.options(),
    )
    .unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    let log = mock.log();
    let sizes: Vec<_> = log
        .iter()
        .filter_map(|r| r.body["children"].as_array().map(Vec::len))
        .collect();
    assert_eq!(sizes, [100, 100, 6]);
}

#[test]
fn nested_blocks_are_appended_after_their_parent_is_created() {
    let mock = Mock::new(|r, _| success(r));
    let dir = tempfile::tempdir().unwrap();
    let mut map = MapStore::open(dir.path().join("map")).unwrap();
    let report = publish(
        &source("- parent\n  - child\n\n| A | B |\n| - | - |\n| x | y |"),
        &Token,
        &mut map,
        &mock.options(),
    )
    .unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    let log = mock.log();
    let appends: Vec<_> = log
        .iter()
        .filter(|r| r.path.contains("/blocks/") && r.method == "PATCH")
        .collect();
    assert_eq!(appends.len(), 3);
    assert!(
        appends[0].body["children"][1]["bulleted_list_item"]
            .get("children")
            .is_none()
    );
    assert_eq!(
        appends[0].body["children"][2]["table"]["children"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(appends[1].path, format!("/v1/blocks/{BLOCK}/children"));
}

#[test]
fn changed_note_failure_invalidates_old_hash_and_rebuilds_old_content_on_return() {
    let mock = Mock::new(|r, index| {
        if index == 5 {
            (403, json!({}))
        } else {
            success(r)
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("map");
    let mut map = MapStore::open(&path).unwrap();
    let options = mock.options();
    assert_eq!(
        publish(&source("old"), &Token, &mut map, &options)
            .unwrap()
            .created,
        ["note"]
    );
    assert_eq!(
        publish(&source("new"), &Token, &mut map, &options)
            .unwrap()
            .failed
            .len(),
        1
    );
    let mut map = MapStore::open(&path).unwrap();
    assert!(map.get("note").unwrap().content_hash.is_empty());
    assert_eq!(
        publish(&source("old"), &Token, &mut map, &options)
            .unwrap()
            .updated,
        ["note"]
    );
}

#[test]
fn retry_budget_and_nonrepeatable_create_failure_are_bounded() {
    let mock = Mock::new(|_, _| (429, json!({})));
    let dir = tempfile::tempdir().unwrap();
    let mut map = MapStore::open(dir.path().join("map")).unwrap();
    let mut options = mock.options();
    options.max_retries = 1;
    let report = publish(&source("hi"), &Token, &mut map, &options).unwrap();
    assert_eq!(report.failed.len(), 1);
    assert_eq!(mock.log().len(), 2);
    assert!(map.entries().is_empty());
    let server_error = Mock::new(|_, _| (503, json!({})));
    let report = publish(&source("hi"), &Token, &mut map, &server_error.options()).unwrap();
    assert_eq!(report.failed.len(), 1);
    assert_eq!(server_error.log().len(), 1);
}

#[test]
fn explicit_archive_flag_trashes_the_existing_page() {
    let mock = Mock::new(|r, _| success(r));
    let dir = tempfile::tempdir().unwrap();
    let mut map = MapStore::open(dir.path().join("map")).unwrap();
    let options = mock.options();
    publish(&source("hello"), &Token, &mut map, &options).unwrap();
    let mut archived = source("hello");
    archived.0[0].archived = true;
    assert_eq!(
        publish(&archived, &Token, &mut map, &options)
            .unwrap()
            .archived,
        ["note"]
    );
    assert_eq!(mock.log().last().unwrap().body["in_trash"], true);
    let count = mock.log().len();
    publish(&archived, &NoToken, &mut map, &options).unwrap();
    assert_eq!(mock.log().len(), count);
}

#[test]
fn archive_failure_leaves_hash_empty_so_returning_note_is_restored() {
    let mock = Mock::new(|r, _| {
        if r.method == "PATCH" && r.body["in_trash"] == true {
            (503, json!({}))
        } else {
            success(r)
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("map");
    let mut map = MapStore::open(&path).unwrap();
    let mut options = mock.options();
    options.max_retries = 0;
    publish(&source("hello"), &Token, &mut map, &options).unwrap();
    let report = publish(&Source(vec![]), &Token, &mut map, &options).unwrap();
    assert_eq!(report.failed.len(), 1);
    let mut map = MapStore::open(&path).unwrap();
    assert!(map.get("note").unwrap().content_hash.is_empty());
    let report = publish(&source("hello"), &Token, &mut map, &options).unwrap();
    assert_eq!(report.updated, ["note"]);
    assert!(report.unchanged.is_empty());
}

#[test]
fn failed_restore_is_archived_if_the_note_disappears_again() {
    let mock = Mock::new(|r, index| {
        if index == 7 {
            (503, json!({}))
        } else {
            success(r)
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("map");
    let mut map = MapStore::open(&path).unwrap();
    let options = mock.options();
    publish(&source("hello"), &Token, &mut map, &options).unwrap();
    publish(&Source(vec![]), &Token, &mut map, &options).unwrap();
    let report = publish(&source("hello"), &Token, &mut map, &options).unwrap();
    assert_eq!(report.failed.len(), 1);
    let mut map = MapStore::open(&path).unwrap();
    let report = publish(&Source(vec![]), &Token, &mut map, &options).unwrap();
    assert_eq!(report.archived, ["note"]);
    assert_eq!(mock.log().last().unwrap().body["in_trash"], true);
}
