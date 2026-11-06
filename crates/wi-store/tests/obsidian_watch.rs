//! Obsidian rewrites `.obsidian/workspace.json` constantly while it runs.
//! With the watcher running, that churn must never become a commit, an
//! index row or a `git status` entry.

mod common;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{Fixture, git, write};
use wi_store::{IngestOutcome, Ingress, MaintainReport, WatchEvent, WatchHandle};

fn head(root: &std::path::Path) -> String {
    git(root, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// Wait until HEAD has not moved for `quiet` (bounded by `limit`).
fn quiet_head(root: &std::path::Path, quiet: Duration, limit: Duration) -> String {
    let deadline = Instant::now() + limit;
    let mut last = head(root);
    let mut since = Instant::now();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        let now = head(root);
        if now != last {
            last = now;
            since = Instant::now();
        } else if since.elapsed() >= quiet {
            return last;
        }
    }
    panic!("HEAD kept moving for {limit:?}");
}

fn drain(
    handle: &WatchHandle,
    events: &mut Vec<WatchEvent>,
    until: impl Fn(&[WatchEvent]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !until(events) {
        if let Ok(event) = handle.events().recv_timeout(Duration::from_millis(50)) {
            events.push(event);
        }
    }
}

fn ingested(events: &[WatchEvent], path: &str) -> bool {
    events.iter().any(|e| {
        matches!(e, WatchEvent::Ingest(r) if r.path == path
            && matches!(r.outcome, IngestOutcome::Ingested { .. }))
    })
}

fn obsidian_status(root: &std::path::Path) -> Vec<String> {
    git(root, &["status", "--porcelain", "--untracked-files=all"])
        .lines()
        .filter(|l| l.contains(".obsidian"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn obsidian_churn_under_a_running_watcher_leaves_no_trace() {
    let (_fx, vault) = Fixture::new();
    let root = vault.root().to_path_buf();
    write(&vault, "seed.md", "seed\n", Ingress::Cli);
    vault.flush().unwrap();
    let (tx, rx) = mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let handle = vault
        .watch_with_hooks(
            wi_store::WatchTuning {
                repair_interval: Duration::from_millis(50),
                debounce: Some(Duration::from_millis(20)),
                maintain_interval: Duration::from_millis(150),
                full_maintain_interval: Duration::from_secs(3600),
                ..Default::default()
            },
            wi_store::WatchHooks {
                on_maintained: Some(Box::new(move |report: &MaintainReport| {
                    let _ = tx.lock().unwrap().send(report.clone());
                })),
            },
        )
        .unwrap();
    rx.recv_timeout(Duration::from_secs(20))
        .expect("the first maintenance pass");
    let tip = quiet_head(&root, Duration::from_secs(1), Duration::from_secs(20));
    let mut events = Vec::new();
    while let Ok(event) = handle.events().try_recv() {
        events.push(event);
    }
    events.clear();

    std::fs::create_dir_all(root.join(".obsidian")).unwrap();
    for n in 0..5 {
        std::fs::write(
            root.join(".obsidian/workspace.json"),
            format!("{{\"active\": \"churnword{n}\"}}"),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(120));
    }
    // Several repair scans and maintenance intervals pass over the churn.
    std::thread::sleep(Duration::from_millis(800));

    // Predicate 1: no commit.
    assert_eq!(head(&root), tip, "Obsidian churn made a commit");
    // Predicate 2: no index row.
    let rows = vault.name_rows().unwrap().unwrap_or_default();
    assert!(
        !rows.iter().any(|r| r.path.starts_with(".obsidian")),
        "{:?}",
        rows.iter().map(|r| &r.path).collect::<Vec<_>>()
    );
    let found = vault
        .search(&wi_store::Query::text("churnword4"), Default::default())
        .unwrap();
    assert!(found.hits.is_empty(), "{:?}", found.hits);
    // Predicate 3: no git status entry.
    assert_eq!(obsidian_status(&root), Vec::<String>::new());

    // Barrier: a later owner note is ingested and committed, so every
    // churn event before it has been handled; none of them was ingested
    // and the only new commit carries the note alone.
    std::fs::write(root.join("sentinel.md"), "sentinel\n").unwrap();
    drain(&handle, &mut events, |e| {
        ingested(e, "sentinel.md")
            && e.iter()
                .any(|e| matches!(e, WatchEvent::Flushed { commits } if *commits > 0))
    });
    handle.stop();
    assert!(ingested(&events, "sentinel.md"), "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WatchEvent::Ingest(r) if r.path.starts_with(".obsidian"))),
        "{events:?}"
    );
    let changed = git(
        &root,
        &["log", "--format=", "--name-only", &format!("{tip}..HEAD")],
    );
    let changed: Vec<&str> = changed.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        !changed.is_empty() && changed.iter().all(|p| *p == "sentinel.md"),
        "{changed:?}"
    );
    assert_eq!(obsidian_status(&root), Vec::<String>::new());
}
