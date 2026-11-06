mod common;

use std::sync::{Arc, Barrier};

use common::{Fixture, assert_deletion_committed, git, write};
use wi_store::{IngestOutcome, Ingress, Query, SearchOptions, Vault};

fn committed(vault: &Vault, path: &str) -> String {
    git(vault.root(), &["log", "--format=%s%n%b", "-1", "--", path])
}

fn unseen_status(vault: &Vault) -> (&'static str, String) {
    let report = vault.doctor().unwrap();
    let check = report
        .checks
        .iter()
        .find(|c| c.name == "unseen")
        .expect("unseen check");
    (check.status, check.detail.clone())
}

fn ingested_paths(report: &wi_store::ReconcileReport) -> Vec<String> {
    report
        .ingested
        .iter()
        .filter(|r| matches!(r.outcome, IngestOutcome::Ingested { .. }))
        .map(|r| r.path.clone())
        .collect()
}

#[test]
fn editor_note_and_attachment_reach_history_after_auto_ingest_and_flush() {
    let (_fx, vault) = Fixture::new();
    std::fs::create_dir_all(vault.root().join("inbox")).unwrap();
    std::fs::write(vault.root().join("inbox/editor.md"), "from an editor\n").unwrap();
    std::fs::write(vault.root().join("inbox/photo.png"), [0u8, 1, 2, 3]).unwrap();
    let report = vault.auto_ingest().unwrap();
    let mut paths = ingested_paths(&report);
    paths.sort();
    assert_eq!(paths, ["inbox/editor.md", "inbox/photo.png"]);
    vault.flush().unwrap();
    for path in ["inbox/editor.md", "inbox/photo.png"] {
        let log = committed(&vault, path);
        assert!(log.contains("Wi-Actor: watcher"), "{path}: {log}");
    }
}

#[test]
fn a_file_indexed_by_search_first_is_still_ingested() {
    let (_fx, vault) = Fixture::new();
    vault.auto_ingest().unwrap();
    std::fs::write(vault.root().join("seen.md"), "indexedword only\n").unwrap();
    vault.catch_up().unwrap();
    let found = vault
        .search(&Query::text("indexedword"), SearchOptions::default())
        .unwrap();
    assert_eq!(found.hits.len(), 1, "the index saw it");
    let report = vault.auto_ingest().unwrap();
    assert_eq!(ingested_paths(&report), ["seen.md"]);
    vault.flush().unwrap();
    assert!(committed(&vault, "seen.md").contains("Wi-Actor: watcher"));
}

#[test]
fn unchanged_vault_hashes_and_ingests_nothing() {
    let (_fx, vault) = Fixture::new();
    for i in 0..20 {
        write(
            &vault,
            &format!("n{i}.md"),
            &format!("note {i}\n"),
            Ingress::Cli,
        );
    }
    vault.flush().unwrap();
    let first = vault.auto_ingest().unwrap();
    assert!(first.ingested.is_empty(), "{first:?}");
    assert!(first.hashed >= 20, "no cache yet: full compare");
    let second = vault.auto_ingest().unwrap();
    assert!(second.ingested.is_empty());
    assert_eq!(second.hashed, 0, "{second:?}");
    assert!(second.scanned >= 20);

    // An engine write moves the branch tip, and the flush stamped the written file: nothing
    // is hashed.
    write(&vault, "n20.md", "written by the engine\n", Ingress::Cli);
    vault.flush().unwrap();
    let third = vault.auto_ingest().unwrap();
    assert!(third.ingested.is_empty(), "{third:?}");
    assert_eq!(third.hashed, 0, "{third:?}");
    assert_eq!(vault.auto_ingest().unwrap().hashed, 0);
}

#[test]
fn a_new_cache_location_means_a_full_compare() {
    let (fx, vault) = Fixture::new();
    write(&vault, "a.md", "a\n", Ingress::Cli);
    vault.flush().unwrap();
    vault.auto_ingest().unwrap();
    assert_eq!(vault.auto_ingest().unwrap().hashed, 0);
    for entry in std::fs::read_dir(fx.cache.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "stat") {
            std::fs::write(&path, "corrupt").unwrap();
        }
    }
    let report = vault.auto_ingest().unwrap();
    assert!(report.hashed >= 1, "{report:?}");
}

#[test]
fn a_tip_moved_outside_the_engine_is_compared_with_the_new_tree() {
    let (_fx, vault) = Fixture::new();
    write(&vault, "kept.md", "kept\n", Ingress::Cli);
    vault.flush().unwrap();
    vault.auto_ingest().unwrap();
    // Shell git drops the file from history while the working file stays.
    git(vault.root(), &["rm", "--cached", "-q", "kept.md"]);
    git(
        vault.root(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "drop",
        ],
    );
    let report = vault.auto_ingest().unwrap();
    assert_eq!(ingested_paths(&report), ["kept.md"], "{report:?}");
    assert_eq!(report.hashed, 1, "stat-unchanged stamps are not hashed");
}

#[test]
fn a_deleted_stamped_file_is_committed_as_an_owner_deletion() {
    let (_fx, vault) = Fixture::new();
    write(&vault, "gone.md", "bye\n", Ingress::Cli);
    vault.flush().unwrap();
    vault.auto_ingest().unwrap();
    std::fs::remove_file(vault.root().join("gone.md")).unwrap();
    let report = vault.auto_ingest().unwrap();
    assert_eq!(report.missing, ["gone.md"]);
    assert_deletion_committed(&vault, "gone.md");
}

#[test]
fn doctor_warns_about_unseen_files_until_they_are_ingested() {
    let (_fx, vault) = Fixture::new();
    assert_eq!(unseen_status(&vault).0, "ok");
    std::fs::write(vault.root().join("draft.md"), "draft\n").unwrap();
    let (status, detail) = unseen_status(&vault);
    assert_eq!(status, "warn");
    assert!(detail.starts_with("1 file(s)"), "{detail}");
    let report = vault.doctor().unwrap();
    assert!(report.healthy(), "a warn is not unhealthy");
    let fix = report
        .checks
        .iter()
        .find(|c| c.name == "unseen")
        .and_then(|c| c.fix.clone())
        .unwrap();
    assert_eq!(
        fix,
        "run any wi command, `wi commit`, or install `wi service`"
    );
    vault.auto_ingest().unwrap();
    assert_eq!(unseen_status(&vault).0, "ok");
}

#[test]
fn two_handles_ingest_one_change_once() {
    let (fx, vault) = Fixture::new();
    std::fs::write(vault.root().join("shared.md"), "one change\n").unwrap();
    drop(vault);
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let barrier = barrier.clone();
            let (root, options) = (fx.root_input(), fx.options());
            std::thread::spawn(move || {
                let vault = Vault::open_with(root, options).unwrap();
                barrier.wait();
                let report = vault.auto_ingest().unwrap();
                vault.flush().unwrap();
                report.ingested.len()
            })
        })
        .collect();
    let total: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
    assert_eq!(total, 1, "exactly one handle publishes the change");
    let vault = fx.open();
    assert_eq!(vault.auto_ingest().unwrap().ingested.len(), 0);
    let log = git(vault.root(), &["log", "--format=%H", "--", "shared.md"]);
    assert_eq!(log.lines().count(), 1, "{log}");
}

#[test]
fn search_right_after_init_and_write_uses_the_index() {
    let (_fx, vault) = Fixture::new();
    assert!(
        vault.index_as_of().unwrap().is_some(),
        "init walked the index"
    );
    write(&vault, "first.md", "firstword\n", Ingress::Cli);
    let found = vault
        .search(&Query::text("firstword"), SearchOptions::default())
        .unwrap();
    assert!(!found.fallback);
    assert!(found.index_as_of.is_some());
    assert_eq!(found.hits.len(), 1);
}

fn lease_file(vault: &Vault) -> std::path::PathBuf {
    vault.root().join(".git/wi/watcher-lease")
}

fn host() -> String {
    let out = std::process::Command::new("hostname").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn write_lease(vault: &Vault, pid: u32, heartbeat_ms: u128) {
    std::fs::write(
        lease_file(vault),
        format!(
            "wi-watcher-lease 1\npid\t{pid}\nhost\t{}\nheartbeat_ms\t{heartbeat_ms}\ntoken\tt\n",
            host()
        ),
    )
    .unwrap();
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

#[test]
fn a_live_watcher_lease_makes_auto_ingest_a_no_op_until_it_stops() {
    let (_fx, vault) = Fixture::new();
    let handle = vault.watch().unwrap();
    assert!(lease_file(&vault).exists(), "the watcher holds the lease");
    let report = vault.auto_ingest().unwrap();
    assert_eq!(report.scanned, 0, "skipped while the watcher runs");
    handle.stop();
    assert!(
        !lease_file(&vault).exists(),
        "a clean stop removes the lease"
    );
    std::fs::write(vault.root().join("after.md"), "after the watcher\n").unwrap();
    let report = vault.auto_ingest().unwrap();
    assert!(report.scanned > 0);
    assert_eq!(ingested_paths(&report), ["after.md"]);
}

#[test]
fn a_lease_with_a_dead_pid_or_an_old_heartbeat_is_ignored() {
    let (_fx, vault) = Fixture::new();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    write_lease(&vault, dead, now_ms());
    assert!(vault.auto_ingest().unwrap().scanned > 0, "dead pid");
    let ttl = wi_store::LEASE_TTL.as_millis();
    write_lease(&vault, std::process::id(), now_ms() - ttl - 1_000);
    assert!(vault.auto_ingest().unwrap().scanned > 0, "stale heartbeat");
    write_lease(&vault, std::process::id(), now_ms());
    assert_eq!(vault.auto_ingest().unwrap().scanned, 0, "fresh lease");
}

#[test]
fn a_file_the_engine_wrote_is_stamped_and_not_hashed_again() {
    let (_fx, vault) = Fixture::new();
    vault.auto_ingest().unwrap();
    write(&vault, "engine.md", "written by the engine\n", Ingress::Cli);
    vault.flush().unwrap();
    let report = vault.auto_ingest().unwrap();
    assert_eq!(report.hashed, 0, "{report:?}");
}

/// The ingress-stat cache file next to the fixture's index database.
fn stat_cache(vault: &Vault) -> std::path::PathBuf {
    let dir = vault.index_database().unwrap().unwrap();
    std::fs::read_dir(dir.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "stat"))
        .expect("ingress cache")
}

#[cfg(unix)]
#[test]
fn a_flush_appends_stamps_instead_of_rewriting_the_cache() {
    use std::os::unix::fs::MetadataExt;
    let (_fx, vault) = Fixture::new();
    vault.auto_ingest().unwrap();
    let cache = stat_cache(&vault);
    let before = std::fs::metadata(&cache).unwrap();
    write(&vault, "one.md", "first\n", Ingress::Cli);
    write(&vault, "two.md", "second\n", Ingress::Cli);
    vault.flush().unwrap();
    let after = std::fs::metadata(&cache).unwrap();
    assert_eq!(after.ino(), before.ino(), "the cache was not replaced");
    assert!(after.len() > before.len(), "stamp lines were appended");
    let report = vault.auto_ingest().unwrap();
    assert_eq!(report.hashed, 0, "{report:?}");
    // That scan recorded the new branch tip; a scan of an unchanged vault
    // then leaves the cache file alone.
    let verified = std::fs::metadata(&cache).unwrap().ino();
    assert_eq!(vault.auto_ingest().unwrap().hashed, 0);
    assert_eq!(std::fs::metadata(&cache).unwrap().ino(), verified);
}

#[test]
fn deleting_an_engine_written_file_is_committed_as_an_owner_deletion() {
    let (_fx, vault) = Fixture::new();
    vault.auto_ingest().unwrap();
    write(&vault, "written.md", "new text\n", Ingress::Cli);
    vault.flush().unwrap();
    std::fs::remove_file(vault.root().join("written.md")).unwrap();
    let report = vault.auto_ingest().unwrap();
    assert_eq!(report.missing, ["written.md"]);
    assert_deletion_committed(&vault, "written.md");
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_is_reported_and_does_not_stop_the_scan() {
    use std::os::unix::fs::PermissionsExt;
    let (_fx, vault) = Fixture::new();
    let locked = vault.root().join("locked.md");
    std::fs::write(&locked, "owner text\n").unwrap();
    std::fs::write(vault.root().join("open.md"), "readable\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable_anyway = std::fs::read(&locked).is_ok();
    let (status, detail) = unseen_status(&vault);
    let report = vault.auto_ingest().unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    if readable_anyway {
        return; // running as root
    }
    assert_eq!(status, "warn");
    assert!(detail.contains("locked.md"), "{detail}");
    assert_eq!(ingested_paths(&report), ["open.md"]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].path, "locked.md");
}

#[test]
fn nested_editor_changes_are_seen_by_the_next_command() {
    let (_fx, vault) = Fixture::new();
    for i in 0..40 {
        write(
            &vault,
            &format!("deep/d{}/e{}/n{i}.md", i % 4, i % 3),
            &format!("note {i}\n"),
            Ingress::Cli,
        );
    }
    std::fs::write(vault.root().join("deep/.gitignore"), "skip/\n").unwrap();
    vault.flush().unwrap();
    vault.auto_ingest().unwrap();
    vault.flush().unwrap();
    let quiet = vault.auto_ingest().unwrap();
    assert_eq!((quiet.hashed, quiet.ingested.len()), (0, 0), "{quiet:?}");

    // An in-place append keeps the directory's mtime; a new file and a
    // deletion in another nested directory change theirs.
    let edited = "deep/d1/e1/n1.md";
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(vault.root().join(edited))
        .unwrap();
    std::io::Write::write_all(&mut file, b"appended in place\n").unwrap();
    drop(file);
    std::fs::create_dir_all(vault.root().join("deep/d2/new/inner")).unwrap();
    std::fs::write(vault.root().join("deep/d2/new/inner/fresh.md"), "fresh\n").unwrap();
    std::fs::create_dir_all(vault.root().join("deep/skip")).unwrap();
    std::fs::write(vault.root().join("deep/skip/ignored.md"), "ignored\n").unwrap();
    std::fs::remove_file(vault.root().join("deep/d3/e0/n3.md")).unwrap();

    let report = vault.auto_ingest().unwrap();
    let mut paths = ingested_paths(&report);
    paths.sort();
    assert_eq!(paths, ["deep/d1/e1/n1.md", "deep/d2/new/inner/fresh.md"]);
    assert_eq!(report.missing, ["deep/d3/e0/n3.md"]);
    vault.flush().unwrap();
    assert!(committed(&vault, edited).contains("Wi-Actor: watcher"));
    assert!(committed(&vault, "deep/skip/ignored.md").trim().is_empty());
    let after = vault.auto_ingest().unwrap();
    assert!(after.ingested.is_empty(), "{after:?}");
}
