//! Regressions for maintenance: resumable time-sliced passes, a configured
//! archive folder, captures left in place, procedure delivery size, and
//! attachment paths that name an engine or Git folder.

mod common;

use std::time::Duration;

use common::{Fixture, git};
use wi_store::{Ingress, MaintainOptions, MaintainReport, Vault};

fn maintain(v: &Vault) -> MaintainReport {
    v.maintain(&MaintainOptions::default()).unwrap()
}

fn put(root: &std::path::Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

fn ingest(v: &Vault) {
    v.auto_ingest().unwrap();
    v.flush().unwrap();
}

fn classify(v: &Vault, path: &str) -> wi_store::Classified {
    v.classify(
        &[(path.into(), std::fs::read(v.root().join(path)).unwrap())],
        &serde_json::Value::Null,
    )
    .unwrap()
    .remove(0)
}

fn capture() -> &'static str {
    "---\ntype: capture\ncapture_status: resolved\noutputs: []\nno_memory_reason: nothing durable\n---\nCaptured evidence.\n"
}

#[test]
fn maintenance_keeps_incoming_markdown_links_and_outgoing_relative_links() {
    let (_f, v) = Fixture::new();
    let root = v.root();
    put(
        root,
        "captures/evidence.md",
        &format!("{}[Attachment](page.html)\n", capture()),
    );
    put(root, "captures/page.html", "<p>source</p>\n");
    put(root, "index.md", "[Evidence](captures/evidence.md)\n");
    ingest(&v);
    let report = maintain(&v);
    println!("maintenance report: {report:?}");
    let incoming = std::fs::read_to_string(root.join("index.md")).unwrap();
    let captured = root.join("captures/evidence.md");
    let incoming_ok = !incoming.contains("(captures/evidence.md)") || captured.is_file();
    let outgoing_ok = std::fs::read_to_string(&captured).is_ok_and(|text| {
        !text.contains("(page.html)") || root.join("captures/page.html").is_file()
    });
    assert!(
        incoming_ok && outgoing_ok,
        "maintenance must preserve Markdown link targets"
    );
}

#[test]
fn maintenance_keeps_a_claim_pinned_to_a_capture_current() {
    let (_f, v) = Fixture::new();
    let root = v.root();
    put(root, "captures/evidence.md", capture());
    ingest(&v);
    let bytes = std::fs::read(root.join("captures/evidence.md")).unwrap();
    let hash = wi_lifecycle::sha256(&bytes);
    put(
        root,
        "claim.md",
        &format!(
            "---\ntype: memory\nkind: fact\nclaim_kind: historical\nstatus: active\nsummary: Recorded evidence\nentities: []\nscope: work\norigin: {{kind: agent}}\nobserved_at: 2026-11-01T00:00:00Z\nsources: [{{ref: captures/evidence.md, commit: null, pending: true, source_hash: {hash}, span_hash: {hash}, locator: {{span: {{start: 0, end: {}}}}}}}]\n---\nRecorded evidence.\n",
            bytes.len()
        ),
    );
    ingest(&v);
    let before = classify(&v, "claim.md");
    assert_eq!(
        before.eligibility, "current",
        "fixture must have citable evidence"
    );
    let report = maintain(&v);
    println!("maintenance report: {report:?}");
    let after = classify(&v, "claim.md");
    assert_eq!(
        after.eligibility, "current",
        "maintenance must preserve pinned evidence: {after:?}"
    );
}

#[test]
fn custom_archive_directory_keeps_archived_procedures_withheld() {
    let (f, v) = Fixture::new();
    let root = v.root().to_path_buf();
    drop(v);
    let cfg = root.join(".wiki-island/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("\narchive_dir = 'retired'\n");
    std::fs::write(&cfg, text).unwrap();
    put(
        &root,
        "retired/p.md",
        "---\ntype: memory\nkind: procedure\nstatus: active\nclaim_kind: intent\nsummary: Retired procedure\nskill_id: retired-procedure\nentities: []\nscope: work\norigin: {kind: user}\n---\nDo the retired thing.\n",
    );
    let v = f.open();
    ingest(&v);
    maintain(&v);
    let classified = classify(&v, "retired/p.md");
    assert_eq!(
        classified.eligibility, "withheld",
        "custom archive must remain archived after upgrade: {classified:?}"
    );
    assert!(root.join("retired/p.md").is_file());
}

#[test]
fn old_capture_folder_stays_and_old_work_folder_migrates_once() {
    let (f, v) = Fixture::new();
    let root = v.root().to_path_buf();
    drop(v);
    let cfg = root.join(".wiki-island/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("\nwork_dir = 'tasks'\ncaptures_dir = 'inbox'\n");
    std::fs::write(&cfg, text).unwrap();
    put(&root, "inbox/evidence.md", capture());
    put(&root, "index.md", "[[inbox/evidence]]\n");
    let work = wi_lifecycle::WorkItem::new(
        wi_lifecycle::WorkKind::Gate,
        "index.md",
        vec![],
        "gate issue",
        "inspect",
        wi_lifecycle::Trigger::Session,
        chrono::Utc::now(),
    );
    put(&root, &work.path("tasks"), &work.render());
    let v = f.open();
    ingest(&v);
    let r = maintain(&v);
    assert_eq!(r.migrated, 1, "only the engine work note moves");
    assert!(root.join("inbox/evidence.md").is_file());
    assert!(root.join(work.path(".wiki-island/work")).is_file());
    let context = v.context("inbox/evidence", 1).unwrap();
    assert!(
        serde_json::to_string(&context)
            .unwrap()
            .contains("index.md")
    );
    let head = git(&root, &["rev-parse", "HEAD"]);
    let generation = v.generation().unwrap();
    let second = maintain(&v);
    assert_eq!(second.migrated, 0);
    assert_eq!(second.work_created, 0);
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
    assert_eq!(v.generation().unwrap(), generation);
}

#[test]
fn procedure_health_flags_a_note_that_cannot_fit_the_mcp_result() {
    let (_f, v) = Fixture::new();
    // The Markdown file is below 16 KiB; JSON escaping doubles its body.
    let text = format!(
        "---\ntype: memory\nkind: procedure\nclaim_kind: intent\nskill_id: quoted-procedure\nsummary: Quote handling\nstatus: active\norigin: {{kind: user}}\nentities: []\nscope: work\n---\n{}\n",
        "\\\"".repeat(6000)
    );
    assert!(text.len() < 16_384);
    put(v.root(), "procedure.md", &text);
    ingest(&v);
    let report = maintain(&v);
    let items = v.work_list().unwrap();
    println!("file bytes: {}; maintenance: {report:?}", text.len());
    assert!(
        items
            .iter()
            .any(|i| i.work_kind.as_deref() == Some("procedure")),
        "an undeliverable procedure must get a health item: {items:?}"
    );
}

#[test]
fn a_small_procedure_gets_no_size_item() {
    let (_f, v) = Fixture::new();
    let text = "---\ntype: memory\nkind: procedure\nclaim_kind: intent\nskill_id: small-procedure\nsummary: Small\nstatus: active\norigin: {kind: user}\nentities: []\nscope: work\n---\nDo the small thing.\n";
    put(v.root(), "procedure.md", text);
    ingest(&v);
    maintain(&v);
    let items = v.work_list().unwrap();
    assert!(
        !items
            .iter()
            .any(|i| i.work_kind.as_deref() == Some("procedure")),
        "{items:?}"
    );
}

#[test]
fn attachments_under_a_nested_engine_or_git_folder_are_refused() {
    let (_f, v) = Fixture::new();
    for path in [
        "notion-imports/x/.wiki-island/config.toml",
        "notion-imports/x/.git/config",
        "a/.Wiki-Island/b.png",
    ] {
        let err = v.write_attachment(path, b"x".to_vec(), None, Ingress::Import);
        assert!(err.is_err(), "{path} must be refused");
        assert!(!v.root().join(path).exists(), "{path} must not be written");
    }
    v.write_attachment(
        "notion-imports/x/a.png",
        b"x".to_vec(),
        None,
        Ingress::Import,
    )
    .unwrap();
}

/// A pass stopped by its time budget resumes where it stopped: slices stay
/// near the budget and a bounded number of them finish the whole pass.
#[test]
fn a_time_sliced_pass_is_bounded_and_eventually_completes() {
    let (_f, v) = Fixture::new();
    let root = v.root();
    let count = std::env::var("WI_SLICE_CORPUS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1500);
    for i in 0..count {
        put(
            root,
            &format!("notes/{i:05}.md"),
            &format!(
                "---\ntype: memory\nkind: event\nstatus: active\nclaim_kind: historical\nsummary: Event {i}\norigin: {{kind: engine}}\nentities: []\nscope: work\n---\nEvent {i}.\n"
            ),
        );
    }
    v.catch_up().unwrap();
    // A whole pass over this corpus takes longer than one slice.
    let budget = Duration::from_millis(
        std::env::var("WI_SLICE_BUDGET_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(150),
    );
    let opts = MaintainOptions {
        full: false,
        time_budget: Some(budget),
    };
    let mut complete = false;
    let mut worst_ms = 0;
    let mut slices = 0;
    let mut procedures_changed = false;
    while slices < 40 {
        let r = v.maintain(&opts).unwrap();
        slices += 1;
        worst_ms = worst_ms.max(r.elapsed_ms);
        procedures_changed |= r.procedures_changed;
        if r.complete {
            complete = true;
            break;
        }
    }
    println!("{count} notes: {slices} slices, worst {worst_ms} ms");
    assert!(complete, "a resumed pass must reach the final steps");
    assert!(procedures_changed, "the procedure step ran");
    assert!(slices > 1, "the corpus must exceed one slice");
    assert!(
        worst_ms < 4 * budget.as_millis() as u64 + 1000,
        "a slice must stay near its budget: {worst_ms} ms"
    );
    // The next pass starts over and is quiet.
    let again = maintain(&v);
    assert!(again.complete);
    assert_eq!(again.work_created, 0);
}

/// The cursor lives on disk: a fresh handle resumes a stopped pass, and a
/// finished pass forgets it.
#[test]
fn a_stopped_pass_resumes_from_another_handle() {
    let (f, v) = Fixture::new();
    for i in 0..300 {
        put(
            v.root(),
            &format!("notes/{i:04}.md"),
            &format!(
                "---\ntype: memory\nkind: event\nstatus: active\nclaim_kind: historical\nsummary: Event {i}\norigin: {{kind: engine}}\nentities: []\nscope: work\n---\nEvent {i}.\n"
            ),
        );
    }
    v.catch_up().unwrap();
    drop(v);
    let cursor = f
        .root_input()
        .join(".wiki-island/local/maintain-cursor.json");
    let opts = MaintainOptions {
        full: false,
        time_budget: Some(Duration::from_nanos(1)),
    };
    let mut slices = 0;
    loop {
        let v = f.open();
        let r = v.maintain(&opts).unwrap();
        slices += 1;
        if r.complete {
            break;
        }
        assert!(cursor.is_file(), "a stopped pass records where it stopped");
        assert!(
            slices < 30,
            "a pass must finish within a bounded number of slices"
        );
    }
    assert!(slices > 2, "{slices}");
    assert!(!cursor.exists(), "a finished pass forgets its cursor");
    let status = git(&f.root_input(), &["status", "--porcelain"]);
    assert!(!status.contains(".wiki-island"), "{status}");
}

/// A temporary ingest failure leaves one work item with a short,
/// vault-relative reason; it closes once the file is in history and opens
/// again when the file fails again.
#[cfg(unix)]
#[test]
fn an_ingest_failure_item_closes_once_the_file_is_in_history() {
    use std::os::unix::fs::PermissionsExt;
    let (_f, v) = Fixture::new();
    maintain(&v);
    let secret = v.root().join("notes/secret.md");
    put(v.root(), "notes/secret.md", "owner text\n");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&secret).is_ok() {
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
        return; // running as root
    }
    ingest(&v);
    maintain(&v);
    let open = |v: &Vault| -> Vec<wi_store::WorkEntry> {
        v.work_list()
            .unwrap()
            .into_iter()
            .filter(|w| w.target.as_deref() == Some("notes/secret.md"))
            .filter(|w| w.state.as_deref() != Some("closed"))
            .collect()
    };
    let items = open(&v);
    assert_eq!(items.len(), 1, "{items:?}");
    let reason = items[0].reason.clone().unwrap_or_default();
    let root = v.root().to_string_lossy().into_owned();
    assert!(reason.contains("permission denied"), "{reason}");
    assert!(
        !reason.contains(&root) && !reason.contains("/private/"),
        "{reason}"
    );
    assert!(!reason.contains("os error"), "{reason}");

    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
    ingest(&v);
    let report = maintain(&v);
    assert!(
        git(v.root(), &["log", "--format=%s", "--", "notes/secret.md"]).contains("notes/secret.md"),
        "not in history"
    );
    assert!(open(&v).is_empty(), "{:?}", open(&v));
    assert!(report.work_closed >= 1, "{report:?}");
    assert!(open(&v).is_empty());
    maintain(&v);
    assert!(open(&v).is_empty(), "stays closed on the next pass");

    // The same failure again reopens the item.
    put(v.root(), "notes/secret.md", "owner text, edited\n");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    ingest(&v);
    maintain(&v);
    let again = open(&v);
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(again.len(), 1, "{again:?}");
}
