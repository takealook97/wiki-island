//! The maintenance pass: idempotence, the built-in procedure, migration of
//! the old folder layout, procedure health items, index self-repair and
//! the time budget.

mod common;

use std::time::Duration;

use common::{Fixture, git};
use wi_store::{Ingress, MaintainOptions, MaintainReport, ProcedureMode, Vault, WriteRequest};

const BUILTIN: &str = ".wiki-island/procedures/wiki-island.maintenance.md";

fn maintain(vault: &Vault) -> MaintainReport {
    vault.maintain(&MaintainOptions::default()).unwrap()
}

fn head(vault: &Vault) -> String {
    git(vault.root(), &["rev-parse", "HEAD"])
}

fn quiet(report: &MaintainReport) -> bool {
    *report
        == MaintainReport {
            elapsed_ms: report.elapsed_ms,
            complete: true,
            ..MaintainReport::default()
        }
}

fn procedure(skill_id: &str, body: &str) -> String {
    format!(
        "---\ntype: memory\nkind: procedure\nskill_id: {skill_id}\nclaim_kind: intent\nsummary: {body}\n---\n{body}\n"
    )
}

fn write_cli(vault: &Vault, path: &str, text: &str) {
    vault
        .lifecycle_write(WriteRequest {
            path: path.into(),
            content: text.as_bytes().to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    vault.flush().unwrap();
}

fn work_paths(vault: &Vault, kind: &str) -> Vec<(String, Option<String>)> {
    vault
        .work_list()
        .unwrap()
        .into_iter()
        .filter(|w| w.work_kind.as_deref() == Some(kind))
        .map(|w| (w.path, w.state))
        .collect()
}

#[test]
fn a_second_pass_changes_nothing_and_commits_nothing() {
    let (_f, vault) = Fixture::new();
    let first = maintain(&vault);
    assert!(first.complete);
    assert!(first.procedures_changed, "{first:?}");
    assert!(vault.root().join(BUILTIN).is_file());
    let tip = head(&vault);
    let generation = vault.generation().unwrap();

    let second = maintain(&vault);
    assert!(quiet(&second), "{second:?}");
    assert_eq!(head(&vault), tip, "no commit when nothing changed");
    assert_eq!(vault.generation().unwrap(), generation);
    let third = maintain(&vault);
    assert!(quiet(&third), "{third:?}");
}

#[test]
fn the_builtin_procedure_is_current_and_refreshed_from_the_binary() {
    let (_f, vault) = Fixture::new();
    maintain(&vault);
    let committed = git(vault.root(), &["show", &format!("HEAD:{BUILTIN}")]);
    assert_eq!(committed, wi_lifecycle::maintenance::note_bytes());
    assert!(committed.contains("kind: engine"));
    let listed = vault.procedures(ProcedureMode::Discovery).unwrap();
    assert!(
        listed
            .iter()
            .any(|p| p.path == BUILTIN && p.eligibility == "current"),
        "{listed:?}"
    );

    // An older binary's text (or a stray edit) is replaced by this one's.
    std::fs::write(vault.root().join(BUILTIN), "---\ntype: memory\n---\nold\n").unwrap();
    let report = maintain(&vault);
    assert!(report.procedures_changed, "{report:?}");
    assert_eq!(
        std::fs::read_to_string(vault.root().join(BUILTIN)).unwrap(),
        wi_lifecycle::maintenance::note_bytes()
    );
    assert!(quiet(&maintain(&vault)));
}

#[test]
fn an_old_layout_moves_under_the_engine_folder_once() {
    let (f, vault) = Fixture::new();
    maintain(&vault);
    let root = vault.root().to_path_buf();
    std::fs::create_dir_all(root.join("work")).unwrap();
    std::fs::create_dir_all(root.join("captures")).unwrap();
    let item = wi_lifecycle::WorkItem::new(
        wi_lifecycle::WorkKind::Recheck,
        "notes/a.md",
        Vec::new(),
        "check it",
        "re-read",
        wi_lifecycle::Trigger::Session,
        chrono::Utc::now(),
    );
    let work_rel = item.path("work");
    std::fs::write(root.join(&work_rel), item.render()).unwrap();
    std::fs::write(root.join("work/plan.md"), "my own plan\n").unwrap();
    std::fs::write(
        root.join("captures/c1.md"),
        "---\ntype: capture\ncapture_status: resolved\noutputs: []\nno_memory_reason: \"nothing durable\"\n---\nraw text\n",
    )
    .unwrap();
    std::fs::write(root.join("captures/page.html"), "<p>pinned</p>\n").unwrap();
    drop(vault);
    let vault = f.open();
    vault.auto_ingest().unwrap();
    vault.flush().unwrap();

    let report = maintain(&vault);
    assert_eq!(report.migrated, 1, "{report:?}");
    let name = work_rel.trim_start_matches("work/");
    assert!(root.join(".wiki-island/work").join(name).is_file());
    assert!(!root.join(&work_rel).exists());
    // Captures, other notes and pinned snapshots stay where they are.
    assert!(root.join("captures/c1.md").is_file());
    assert!(!root.join(".wiki-island/captures/c1.md").exists());
    assert!(root.join("work/plan.md").is_file());
    assert!(root.join("captures/page.html").is_file());
    let status = git(&root, &["status", "--porcelain"]);
    assert!(status.trim().is_empty(), "{status}");

    let again = maintain(&vault);
    assert_eq!(again.migrated, 0, "{again:?}");
    assert!(quiet(&maintain(&vault)));
}

#[test]
fn procedure_health_items_are_stable_and_close_when_the_cause_is_gone() {
    let (_f, vault) = Fixture::new();
    write_cli(
        &vault,
        "skills/a.md",
        &procedure("deploy", "Deploy with care"),
    );
    write_cli(
        &vault,
        "skills/b.md",
        &procedure("deploy", "Deploy quickly"),
    );
    let first = maintain(&vault);
    let items = work_paths(&vault, "procedure");
    assert_eq!(items.len(), 2, "{first:?} {items:?}");
    assert!(first.work_created >= 2, "{first:?}");
    let tip = head(&vault);
    assert!(quiet(&maintain(&vault)));
    assert_eq!(
        work_paths(&vault, "procedure"),
        items,
        "same keys every pass"
    );
    assert_eq!(head(&vault), tip);

    // Fix the duplicate: the items close themselves.
    let read = vault.read("skills/b.md").unwrap();
    vault
        .lifecycle_write(WriteRequest {
            path: "skills/b.md".into(),
            content: procedure("deploy-fast", "Deploy quickly").into_bytes(),
            expected_checksum: Some(read.checksum),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    vault.flush().unwrap();
    let fixed = maintain(&vault);
    assert_eq!(fixed.work_closed, 2, "{fixed:?}");
    assert!(
        work_paths(&vault, "procedure")
            .iter()
            .all(|(_, state)| state.as_deref() == Some("closed"))
    );
    assert!(quiet(&maintain(&vault)));
}

#[test]
fn a_lost_index_is_rebuilt_without_asking_anyone() {
    let (f, vault) = Fixture::new();
    write_cli(&vault, "notes/a.md", "alpha note\n");
    maintain(&vault);
    drop(vault);
    let db = f.cache.path().join("index.sqlite3");
    std::fs::remove_file(&db).unwrap();
    let vault = f.open();
    let report = maintain(&vault);
    assert!(report.index_rebuilt, "{report:?}");
    assert!(quiet(&maintain(&vault)));
}

#[test]
fn a_spent_budget_stops_early_and_the_next_pass_finishes() {
    let (_f, vault) = Fixture::new();
    let stopped = vault
        .maintain(&MaintainOptions {
            full: false,
            time_budget: Some(Duration::ZERO),
        })
        .unwrap();
    assert!(!stopped.complete);
    assert!(!vault.root().join(BUILTIN).exists());
    let resumed = vault
        .maintain(&MaintainOptions {
            full: true,
            time_budget: Some(Duration::from_secs(60)),
        })
        .unwrap();
    assert!(resumed.complete);
    assert!(vault.root().join(BUILTIN).is_file());
}

#[test]
fn obsidian_settings_are_never_ingested_or_committed() {
    let (_f, vault) = Fixture::new();
    maintain(&vault);
    let root = vault.root().to_path_buf();
    std::fs::create_dir_all(root.join(".obsidian")).unwrap();
    std::fs::create_dir_all(root.join(".trash")).unwrap();
    std::fs::write(root.join("note.md"), "Obsidian note with [[link]]\n").unwrap();
    vault.auto_ingest().unwrap();
    vault.flush().unwrap();
    let tip = head(&vault);
    for n in 0..3 {
        std::fs::write(
            root.join(".obsidian/workspace.json"),
            format!("{{\"active\": {n}}}"),
        )
        .unwrap();
        std::fs::write(root.join(".trash/old.md"), format!("deleted {n}\n")).unwrap();
        let report = vault.auto_ingest().unwrap();
        assert!(report.ingested.is_empty(), "{report:?}");
        vault.flush().unwrap();
    }
    assert_eq!(head(&vault), tip, "Obsidian churn makes no commit");
    assert!(quiet(
        &vault
            .maintain(&MaintainOptions {
                full: true,
                time_budget: None
            })
            .unwrap()
    ));
    let tracked = git(&root, &["ls-files"]);
    assert!(!tracked.contains(".obsidian") && !tracked.contains(".trash"));
    let query = wi_store::Query {
        text: "deleted OR Obsidian".into(),
        ..Default::default()
    };
    let hits = vault.search(&query, Default::default()).unwrap();
    let paths: Vec<String> = hits
        .hits
        .iter()
        .map(|h| h.rel_path.display().to_string())
        .collect();
    assert!(paths.contains(&"note.md".to_owned()), "{paths:?}");
    assert!(!paths.iter().any(|p| p.starts_with(".trash")), "{paths:?}");
    let r = vault.write(WriteRequest {
        path: ".obsidian/app.json".into(),
        content: b"{}".to_vec(),
        expected_checksum: None,
        op_id: None,
        ingress: Ingress::Cli,
    });
    assert_eq!(r.unwrap_err().code(), "protected-path");
}

#[test]
fn the_watcher_maintains_after_its_first_repair_and_then_only_on_change() {
    use std::sync::mpsc;
    let (_f, vault) = Fixture::new();
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
    let wait = Duration::from_secs(10);
    let first = rx
        .recv_timeout(wait)
        .expect("first pass after the first repair");
    assert!(first.procedures_changed, "{first:?}");
    assert!(vault.root().join(BUILTIN).is_file());
    // Idle: the generation does not change, so no pass runs.
    assert!(rx.recv_timeout(Duration::from_millis(600)).is_err());
    // A change makes the next interval run a pass.
    std::fs::write(
        vault.root().join("skill.md"),
        procedure("release", "Release the build"),
    )
    .unwrap();
    let next = rx.recv_timeout(wait).expect("a pass after a change");
    assert!(next.complete, "{next:?}");
    handle.stop();
}

fn put(vault: &Vault, path: &str, text: &str) {
    let path = vault.root().join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn ingest(vault: &Vault) {
    vault.auto_ingest().unwrap();
    vault.flush().unwrap();
}

fn background(vault: &Vault) {
    let words = (0..200)
        .map(|i| format!("common{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    put(vault, "background.md", &format!("{words}\n").repeat(100));
}

fn workflow(group: usize) -> String {
    (0..25)
        .map(|i| format!("workflow{group}term{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn candidate_group(vault: &Vault, group: usize) {
    for member in 0..3 {
        put(
            vault,
            &format!("notes/group-{group}-{member}.md"),
            &format!(
                "---\ntitle: Workflow cluster{group}\ndate: 2026-11-0{}\n---\n{} member{member}\n",
                member + 1,
                workflow(group),
            ),
        );
    }
}

fn duplicate_group(vault: &Vault, group: usize) {
    let body =
        format!("Duplicate group {group} contains enough meaningful body content. ").repeat(6);
    for (member, body) in [body.clone(), body.to_uppercase(), body.replace(' ', "  ")]
        .iter()
        .enumerate()
    {
        put(vault, &format!("copies/group-{group}-{member}.md"), body);
    }
}

#[test]
fn mixed_vault_suggestions_are_quiet_and_keep_duplicate_group_keys() {
    let (_f, vault) = Fixture::new();
    // Two hundred notes: shared daily/meeting headings, unique notes, two
    // real duplicate groups, a repeated workflow and vault boilerplate.
    for i in 0..50 {
        put(
            &vault,
            &format!("daily/2026-01-{}.md", i + 1),
            &format!(
                "---\ntitle: Daily note\ndate: 2026-01-01\n---\n## Notes\nDaily observation {i}\n"
            ),
        );
        put(
            &vault,
            &format!("meetings/sync-{i}.md"),
            &format!(
                "---\ntitle: Weekly sync\ndate: 2026-01-01\n---\n# Weekly sync\nMeeting observation {i}\n"
            ),
        );
    }
    for i in 0..90 {
        put(
            &vault,
            &format!("unique/{i}.md"),
            &format!("Unique subject {i}\n"),
        );
    }
    for group in 0..2 {
        duplicate_group(&vault, group);
    }
    candidate_group(&vault, 0);
    background(&vault);
    ingest(&vault);
    let first = maintain(&vault);
    assert_eq!(work_paths(&vault, "duplicate").len(), 2, "{first:?}");
    assert_eq!(
        work_paths(&vault, "promote-candidate").len(),
        1,
        "{first:?}"
    );
    assert_eq!(first.work_created, 3, "{first:?}");
    let ids = work_paths(&vault, "duplicate");
    let tip = head(&vault);
    assert!(quiet(&maintain(&vault)));
    assert_eq!(head(&vault), tip);

    // Removing the oldest member refreshes the same body-derived item.
    for i in 0..8 {
        std::fs::remove_file(vault.root().join(format!("daily/2026-01-{}.md", i + 1))).unwrap();
    }
    for group in 0..2 {
        std::fs::remove_file(vault.root().join(format!("copies/group-{group}-0.md"))).unwrap();
    }
    ingest(&vault);
    let refreshed = maintain(&vault);
    // External deletion also produces ordinary missing-file work; detector
    // identity remains stable independently of those recovery notifications.
    assert_eq!(work_paths(&vault, "promote-candidate").len(), 1);
    assert_eq!(refreshed.work_closed, 0, "{refreshed:?}");
    assert_eq!(work_paths(&vault, "duplicate"), ids);
    let tip = head(&vault);
    assert!(quiet(&maintain(&vault)));
    assert_eq!(head(&vault), tip);

    for group in 0..2 {
        std::fs::remove_file(vault.root().join(format!("copies/group-{group}-1.md"))).unwrap();
    }
    for member in 1..3 {
        std::fs::remove_file(vault.root().join(format!("notes/group-0-{member}.md"))).unwrap();
    }
    ingest(&vault);
    let resolved = maintain(&vault);
    assert_eq!(resolved.work_closed, 3, "{resolved:?}");
    assert!(quiet(&maintain(&vault)));
}

#[test]
fn duplicate_creation_is_capped_without_closing_live_groups() {
    let (_f, vault) = Fixture::new();
    for group in 0..7 {
        duplicate_group(&vault, group);
    }
    ingest(&vault);
    let first = maintain(&vault);
    assert_eq!(work_paths(&vault, "duplicate").len(), 5, "{first:?}");
    let second = maintain(&vault);
    assert_eq!(second.work_created, 2, "{second:?}");
    assert_eq!(second.work_closed, 0);
    assert_eq!(work_paths(&vault, "duplicate").len(), 7);
    let tip = head(&vault);
    assert!(quiet(&maintain(&vault)));
    assert_eq!(head(&vault), tip);
}

#[test]
fn promotion_caps_limit_new_and_total_open_items_without_closing_live_causes() {
    let (_f, vault) = Fixture::new();
    background(&vault);
    for group in 0..12 {
        candidate_group(&vault, group);
    }
    ingest(&vault);
    for expected in [3, 6, 9, 10] {
        let report = maintain(&vault);
        assert!(report.work_created <= 3, "{report:?}");
        assert_eq!(report.work_closed, 0);
        assert_eq!(work_paths(&vault, "promote-candidate").len(), expected);
    }
    let tip = head(&vault);
    assert!(quiet(&maintain(&vault)));
    assert_eq!(head(&vault), tip);
    assert!(
        work_paths(&vault, "promote-candidate")
            .iter()
            .all(|(_, state)| state.as_deref() == Some("ready"))
    );

    let item = vault
        .work_list()
        .unwrap()
        .into_iter()
        .find(|w| w.work_kind.as_deref() == Some("promote-candidate"))
        .unwrap();
    vault
        .work_close(&wi_store::WorkClose {
            path: item.path,
            outcome: wi_lifecycle::Outcome::Adopted,
            limits: None,
            reopen_trigger: None,
            reopen_detail: None,
        })
        .unwrap();
    vault.flush().unwrap();
    let report = maintain(&vault);
    assert_eq!(
        report.work_created, 1,
        "a closed item frees one slot: {report:?}"
    );
    assert_eq!(
        work_paths(&vault, "promote-candidate")
            .iter()
            .filter(|(_, state)| state.as_deref() != Some("closed"))
            .count(),
        10
    );
    assert!(quiet(&maintain(&vault)));
}

#[test]
fn note_targeted_duplicate_items_close_when_only_a_heading_matches() {
    let (_f, vault) = Fixture::new();
    for name in ["a", "b"] {
        put(
            &vault,
            &format!("notes/{name}.md"),
            &format!("---\ntitle: Daily note\n---\n## Notes\nDifferent body {name}\n"),
        );
    }
    let item = wi_lifecycle::WorkItem::new(
        wi_lifecycle::WorkKind::Duplicate,
        "notes/b.md",
        vec!["notes/a.md".into()],
        "Shared heading",
        "Compare the notes",
        wi_lifecycle::Trigger::Session,
        chrono::Utc::now(),
    )
    .with_condition("duplicate: shared heading");
    put(&vault, &item.path(".wiki-island/work"), &item.render());
    ingest(&vault);
    let report = maintain(&vault);
    assert_eq!(report.work_created, 0, "{report:?}");
    assert_eq!(report.work_closed, 1, "{report:?}");
    assert_eq!(
        work_paths(&vault, "duplicate"),
        vec![(item.path(".wiki-island/work"), Some("closed".into()))]
    );
    let tip = head(&vault);
    assert!(quiet(&maintain(&vault)));
    assert_eq!(head(&vault), tip);
}

fn commits(vault: &Vault) -> usize {
    git(vault.root(), &["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn one_pass_publishes_all_its_changes_as_one_commit() {
    let (_f, vault) = Fixture::new();
    // The built-in procedure is missing and three duplicate groups exist:
    // one pass refreshes the procedure and creates three items.
    std::fs::remove_file(vault.root().join(BUILTIN)).ok();
    for group in 0..3 {
        duplicate_group(&vault, group);
    }
    ingest(&vault);
    let before = commits(&vault);
    let created = maintain(&vault);
    assert_eq!(created.work_created, 3, "{created:?}");
    assert!(created.procedures_changed, "{created:?}");
    assert_eq!(commits(&vault), before + 1, "{created:?}");
    assert!(git(vault.root(), &["status", "--porcelain"]).is_empty());
    assert!(quiet(&maintain(&vault)));
    assert_eq!(commits(&vault), before + 1);

    // Every group loses a member's copy: three items close in one commit.
    for group in 0..3 {
        for member in 1..3 {
            std::fs::remove_file(
                vault
                    .root()
                    .join(format!("copies/group-{group}-{member}.md")),
            )
            .unwrap();
        }
    }
    ingest(&vault);
    let before = commits(&vault);
    let closed = maintain(&vault);
    assert_eq!(closed.work_closed, 3, "{closed:?}");
    assert_eq!(commits(&vault), before + 1, "{closed:?}");
    assert!(quiet(&maintain(&vault)));
    assert_eq!(commits(&vault), before + 1);
}
