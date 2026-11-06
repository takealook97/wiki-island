mod common;

use std::rc::Rc;
use std::time::{Duration, Instant};

use common::{Fixture, git, manifest_text, write};
use wi_store::{Hooks, IngestOutcome, Ingress, Query, SearchOptions, WatchEvent, WatchHandle};

/// Collect watcher events until `done` holds or the deadline passes, then
/// keep draining for a quiet period.
fn wait_for(
    handle: &WatchHandle,
    timeout: Duration,
    done: impl Fn(&[WatchEvent]) -> bool,
) -> Vec<WatchEvent> {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    while Instant::now() < deadline && !done(&events) {
        if let Ok(event) = handle.events().recv_timeout(Duration::from_millis(50)) {
            events.push(event);
        }
    }
    let quiet = Instant::now() + Duration::from_millis(600);
    while Instant::now() < quiet {
        if let Ok(event) = handle.events().recv_timeout(Duration::from_millis(50)) {
            events.push(event);
        }
    }
    events
}

fn ingested(events: &[WatchEvent], path: &str) -> bool {
    events.iter().any(|e| {
        matches!(e, WatchEvent::Ingest(r) if r.path == path && matches!(r.outcome, IngestOutcome::Ingested { .. }))
    })
}

fn flushed(events: &[WatchEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, WatchEvent::Flushed { commits } if *commits > 0))
}

#[test]
fn external_editor_change_is_indexed_and_committed_after_the_debounce() {
    let (_fx, vault) = Fixture::new();
    write(&vault, "seed.md", "seed\n", Ingress::Cli);
    vault.flush().unwrap();
    vault.catch_up().unwrap();
    let handle = vault.watch().unwrap();
    std::fs::create_dir_all(vault.root().join("inbox")).unwrap();
    std::fs::write(
        vault.root().join("inbox/editor.md"),
        "editorword from an editor\n",
    )
    .unwrap();
    let events = wait_for(&handle, Duration::from_secs(10), |e| {
        ingested(e, "inbox/editor.md") && flushed(e)
    });
    assert!(ingested(&events, "inbox/editor.md"), "{events:?}");
    assert!(flushed(&events), "{events:?}");
    handle.stop();
    let found = vault
        .search(&Query::text("editorword"), SearchOptions::default())
        .unwrap();
    assert_eq!(found.hits.len(), 1, "indexed by the watcher, no catch-up");
    let log = git(
        vault.root(),
        &["log", "--format=%s%n%b", "-1", "--", "inbox/editor.md"],
    );
    assert!(log.contains("Wi-Actor: watcher"), "{log}");
}

#[cfg(target_os = "linux")]
#[test]
fn populated_directory_moved_into_the_vault_is_ingested() {
    let (fx, vault) = Fixture::new();
    write(&vault, "seed.md", "seed\n", Ingress::Cli);
    vault.flush().unwrap();
    let staged = fx.dir.path().join("staged");
    std::fs::create_dir_all(staged.join("nested")).unwrap();
    std::fs::write(staged.join("nested/editor.md"), "movedword\n").unwrap();
    let handle = vault.watch().unwrap();
    // The child already exists before inotify can watch the arriving directory.
    std::fs::rename(&staged, vault.root().join("inbox")).unwrap();
    let events = wait_for(&handle, Duration::from_secs(10), |e| {
        ingested(e, "inbox/nested/editor.md") && flushed(e)
    });
    assert!(ingested(&events, "inbox/nested/editor.md"), "{events:?}");
    assert!(flushed(&events), "{events:?}");
    handle.stop();
    assert_eq!(
        vault
            .search(&Query::text("movedword"), SearchOptions::default())
            .unwrap()
            .hits
            .len(),
        1,
        "directory ingress updates the index"
    );
    let log = git(
        vault.root(),
        &["log", "--format=%b", "-1", "--", "inbox/nested/editor.md"],
    );
    assert!(log.contains("Wi-Actor: watcher"), "{log}");
}

#[test]
fn watcher_ignores_an_engine_write_in_flight_and_ingests_an_editor_save() {
    let (_fx, mut vault) = Fixture::new();
    write(&vault, "seed.md", "seed\n", Ingress::Cli);
    vault.flush().unwrap();
    let handle = vault.watch().unwrap();
    // Hold the intent open long enough for the watcher to see the new file
    // before the `done` record exists.
    vault.set_hooks(Hooks {
        after_swap: Some(Rc::new(|_| std::thread::sleep(Duration::from_millis(800)))),
        ..Hooks::default()
    });
    let result = write(
        &vault,
        "agent.md",
        "---\ntype: memory\nsummary: s\n---\nagent text\n",
        Ingress::Mcp,
    );
    vault.set_hooks(Hooks::default());
    let events = wait_for(&handle, Duration::from_secs(3), |e| {
        e.iter()
            .any(|e| matches!(e, WatchEvent::Ingest(r) if r.path == "agent.md"))
    });
    assert!(
        events.iter().any(|e| matches!(e, WatchEvent::Ingest(r) if r.path == "agent.md" && r.outcome == IngestOutcome::Unchanged)),
        "the watcher saw the engine write and correlated it: {events:?}"
    );
    assert!(!ingested(&events, "agent.md"), "{events:?}");
    let manifest = manifest_text(&vault);
    assert!(
        !manifest.contains("\"kind\":\"ingest\",\"seq\""),
        "no ingest record: {manifest}"
    );
    // An unmatched editor save in the same run is ingested with actor watcher.
    std::fs::write(vault.root().join("human.md"), "human text\n").unwrap();
    let events = wait_for(&handle, Duration::from_secs(10), |e| {
        ingested(e, "human.md") && flushed(e)
    });
    assert!(ingested(&events, "human.md"), "{events:?}");
    handle.stop();
    vault.flush().unwrap();
    let text = std::fs::read_to_string(vault.root().join("agent.md")).unwrap();
    assert!(text.contains("kind: agent"), "{text}");
    let agent_log = git(vault.root(), &["log", "--format=%b", "--", "agent.md"]);
    assert!(agent_log.contains("Wi-Actor: mcp"), "{agent_log}");
    assert!(!agent_log.contains("Wi-Actor: watcher"), "{agent_log}");
    let human_log = git(vault.root(), &["log", "--format=%b", "--", "human.md"]);
    assert!(human_log.contains("Wi-Actor: watcher"), "{human_log}");
    let _ = result;
}
