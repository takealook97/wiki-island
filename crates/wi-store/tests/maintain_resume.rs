//! Budget-sliced maintenance across vault changes, process exits and an
//! archive directory change.

mod common;
use common::Fixture;
use std::time::Duration;
use wi_store::{Ingress, MaintainOptions, Vault};
fn put(v: &Vault, p: &str, s: &str) {
    let p = v.root().join(p);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}
fn ingest(v: &Vault) {
    v.auto_ingest().unwrap();
    v.flush().unwrap();
}
fn opts() -> MaintainOptions {
    MaintainOptions {
        full: false,
        time_budget: Some(Duration::from_nanos(1)),
    }
}
fn cursor(v: &Vault) -> serde_json::Value {
    serde_json::from_slice(
        &std::fs::read(v.root().join(".wiki-island/local/maintain-cursor.json")).unwrap(),
    )
    .unwrap()
}
fn scan_start(v: &Vault) {
    for _ in 0..30 {
        let r = v.maintain(&opts()).unwrap();
        assert!(!r.complete);
        let c = cursor(v);
        if c["stage"] == "scan" && c["offset"].as_u64().unwrap() > 0 {
            return;
        }
    }
    panic!("scan not reached")
}
fn finish(v: &Vault) {
    for _ in 0..40 {
        if v.maintain(&opts()).unwrap().complete {
            return;
        }
    }
    panic!("never complete")
}
fn seed(v: &Vault) {
    for i in 0..200 {
        put(
            v,
            &format!("notes/{i:04}.md"),
            &format!(
                "---\ntype: memory\nkind: event\nstatus: active\nclaim_kind: historical\nsummary: Event {i}\norigin: {{kind: engine}}\nentities: []\nscope: work\n---\nEvent {i}.\n"
            ),
        );
    }
    ingest(v);
}
#[test]
fn changes_between_scan_slices_are_not_lost() {
    let (_f, v) = Fixture::new();
    seed(&v);
    scan_start(&v);
    let before = v.generation().unwrap();
    for path in ["notes/0000.md", "notes/0001.md"] {
        let checksum = v.read(path).unwrap().checksum;
        v.write(wi_store::WriteRequest{path:path.into(),content:b"---\ntype: memory\nkind: event\nstatus: active\nclaim_kind: historical\nsummary: Shared replacement\norigin: {kind: engine}\nentities: []\nscope: work\n---\nShared replacement body that is long enough for the duplicate detector to compare. Shared replacement body that is long enough for the duplicate detector to compare. Shared replacement body that is long enough for the duplicate detector to compare. \n".to_vec(),expected_checksum:Some(checksum),op_id:None,ingress:Ingress::Engine}).unwrap();
    }
    v.flush().unwrap();
    assert!(v.generation().unwrap() > before);
    finish(&v);
    let sliced = v.work_list().unwrap();
    let missed = !sliced
        .iter()
        .any(|x| x.work_kind.as_deref() == Some("duplicate"));
    let full = v.maintain(&MaintainOptions::default()).unwrap();
    let after = v.work_list().unwrap();
    println!(
        "generation {before}->{}, duplicate after sliced={}, after fresh={}, fresh={full:?}",
        v.generation().unwrap(),
        !missed,
        after
            .iter()
            .any(|x| x.work_kind.as_deref() == Some("duplicate"))
    );
    assert!(
        after
            .iter()
            .any(|x| x.work_kind.as_deref() == Some("duplicate")),
        "fixture must have a duplicate"
    );
    assert!(
        !missed,
        "generation changes between slices must not be lost by stale scan input"
    );
}
#[test]
fn legacy_path_archiving_survives_archive_directory_change() {
    let (f, v) = Fixture::new();
    put(
        &v,
        "p.md",
        "---\ntype: memory\nkind: procedure\nstatus: active\nclaim_kind: intent\nskill_id: archive-check\nsummary: Archived procedure\norigin: {kind: user}\n---\nOld procedure.\n",
    );
    ingest(&v);
    v.archive("p.md", None, None, Ingress::Cli).unwrap();
    v.flush().unwrap();
    let bytes = std::fs::read(v.root().join("archive/p.md")).unwrap();
    println!("archive bytes={}", String::from_utf8_lossy(&bytes));
    drop(v);
    let cfg = f.root_input().join(".wiki-island/config.toml");
    let mut s = std::fs::read_to_string(&cfg).unwrap();
    s.push_str("\narchive_dir = 'retired'\n");
    std::fs::write(cfg, s).unwrap();
    let v = f.open();
    ingest(&v);
    v.maintain(&MaintainOptions::default()).unwrap();
    let c = v
        .classify(&[("archive/p.md".into(), bytes)], &serde_json::Value::Null)
        .unwrap();
    println!("after config change={c:?}");
    assert_eq!(c[0].eligibility, "withheld");
}
#[test]
#[ignore]
fn crash_helper() {
    let root = std::env::var("R2_CRASH_ROOT").unwrap();
    let index = std::env::var("R2_CRASH_INDEX").unwrap();
    let v = Vault::open_with(
        root,
        wi_store::OpenOptions {
            index_path: Some(index.into()),
        },
    )
    .unwrap();
    scan_start(&v);
    std::process::exit(77)
}
#[test]
fn cursor_survives_abrupt_process_exit_and_finishes_without_duplicate_work() {
    let (f, v) = Fixture::new();
    seed(&v);
    let root = v.root().to_path_buf();
    drop(v);
    let s = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_helper", "--ignored", "--nocapture"])
        .env("R2_CRASH_ROOT", &root)
        .env("R2_CRASH_INDEX", f.cache.path().join("index.sqlite3"))
        .status()
        .unwrap();
    assert_eq!(s.code(), Some(77));
    let v = f.open();
    println!("after crash cursor={}", cursor(&v));
    finish(&v);
    assert!(
        !root
            .join(".wiki-island/local/maintain-cursor.json")
            .exists()
    );
    let again = v.maintain(&MaintainOptions::default()).unwrap();
    assert_eq!(again.work_created, 0);
    assert_eq!(again.work_closed, 0);
}

#[test]
fn lifecycle_archiving_survives_archive_directory_change() {
    let (f, v) = Fixture::new();
    put(
        &v,
        "p.md",
        "---\ntype: memory\nkind: procedure\nstatus: active\nclaim_kind: intent\nskill_id: archive-check\nsummary: Archived procedure\norigin: {kind: user}\n---\nOld procedure.\n",
    );
    ingest(&v);
    v.lifecycle_archive("p.md", None, None, Ingress::Cli)
        .unwrap();
    v.flush().unwrap();
    let bytes = std::fs::read(v.root().join("archive/p.md")).unwrap();
    drop(v);
    let cfg = f.root_input().join(".wiki-island/config.toml");
    let mut s = std::fs::read_to_string(&cfg).unwrap();
    s.push_str("\narchive_dir = 'retired'\n");
    std::fs::write(cfg, s).unwrap();
    let v = f.open();
    ingest(&v);
    v.maintain(&MaintainOptions::default()).unwrap();
    let c = v
        .classify(&[("archive/p.md".into(), bytes)], &serde_json::Value::Null)
        .unwrap();
    println!("lifecycle archive after config change={c:?}");
    assert_eq!(c[0].eligibility, "withheld");
}

#[test]
fn a_reopened_handle_rescans_in_chunks() {
    let (f, v) = Fixture::new();
    seed(&v);
    scan_start(&v);
    drop(v);
    let v = f.open();
    let r = v.maintain(&opts()).unwrap();
    assert!(!r.complete, "{r:?}");
    let c = cursor(&v);
    assert_eq!(c["stage"], "scan", "{c}");
    assert_eq!(c["offset"], 64, "one chunk per slice after a reopen: {c}");
    finish(&v);
    assert_eq!(
        v.maintain(&MaintainOptions::default())
            .unwrap()
            .work_created,
        0
    );
}

#[test]
fn the_old_work_folder_migrates_in_chunks() {
    let (_f, v) = Fixture::new();
    v.maintain(&MaintainOptions::default()).unwrap();
    std::fs::create_dir_all(v.root().join("work")).unwrap();
    for i in 0..150 {
        let item = wi_lifecycle::WorkItem::new(
            wi_lifecycle::WorkKind::Recheck,
            format!("notes/{i:04}.md"),
            Vec::new(),
            "check it",
            "re-read",
            wi_lifecycle::Trigger::Session,
            chrono::Utc::now(),
        );
        std::fs::write(v.root().join(item.path("work")), item.render()).unwrap();
    }
    ingest(&v);
    let mut migrated = Vec::new();
    for _ in 0..40 {
        let r = v.maintain(&opts()).unwrap();
        if r.migrated > 0 {
            migrated.push(r.migrated);
        }
        if r.complete {
            break;
        }
    }
    assert_eq!(migrated.iter().sum::<usize>(), 150, "{migrated:?}");
    assert!(migrated.iter().all(|&n| n <= 64), "{migrated:?}");
    assert!(!v.root().join("work").exists());
}
