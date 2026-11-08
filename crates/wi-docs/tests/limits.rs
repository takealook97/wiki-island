//! Process-isolated environment overrides; no mutation of the test runner's PATH.
#![cfg(unix)]
use lopdf::{Document, Object, Stream, dictionary};
use std::{
    io::{Cursor, Write},
    os::unix::fs::PermissionsExt,
    process::Command,
    time::{Duration, Instant},
};
use wi_docs::{ImportOptions, Ocr, Status, import};
use wi_store::{OpenOptions, Vault};

fn blank_pdf() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages = doc.new_object_id();
    let content = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
    let mut kids = Vec::new();
    for _ in 0..3 {
        let page = doc.add_object(dictionary! {"Type"=>"Page", "Parent"=>pages, "Contents"=>content, "MediaBox"=>vec![0.into(),0.into(),612.into(),792.into()]});
        kids.push(page.into());
    }
    doc.objects.insert(
        pages,
        Object::Dictionary(dictionary! {"Type"=>"Pages", "Kids"=>kids, "Count"=>3}),
    );
    let root = doc.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
    doc.trailer.set("Root", root);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn isolated_import_child() {
    let Ok(mode) = std::env::var("WI_DOCS_LIMIT_CASE") else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os("WI_VAULT").unwrap());
    let (vault, _) = Vault::init_with(
        &root,
        OpenOptions {
            index_path: Some(root.with_extension("sqlite3")),
        },
    )
    .unwrap();
    std::fs::write(vault.root().join("blank.pdf"), blank_pdf()).unwrap();
    let started = Instant::now();
    let report = import(
        &vault,
        "blank.pdf",
        ImportOptions {
            ocr: Ocr::Off,
            bulk: false,
        },
    )
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "unbounded tool: {:?}",
        started.elapsed()
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.contains("tool-timeout")),
        "{:?}",
        report.diagnostics
    );
    assert!(vault.read(&report.sidecar_path).is_ok());
    if mode == "budget" {
        assert_eq!(report.status, Status::Partial);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.contains("import-budget"))
        );
    } else {
        assert_eq!(report.status, Status::NeedsOcr);
        assert_eq!(report.pages, 3);
    }
}

fn child(mode: &str, budget: &str) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("pdftotext");
    let script_body = if mode == "discovery" {
        "#!/bin/sh\nexec /bin/sleep 2\n"
    } else {
        "#!/bin/sh\nif [ \"$1\" = \"-v\" ]; then echo fake-tool; exit 0; fi\nexec /bin/sleep 2\n"
    };
    std::fs::write(&script, script_body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "isolated_import_child", "--nocapture"])
        .env("WI_DOCS_LIMIT_CASE", mode)
        .env("WI_TOOL_TIMEOUT_MS", "200")
        .env("WI_IMPORT_BUDGET_MS", budget)
        .env("PATH", dir.path())
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .env("APPDATA", dir.path())
        .env("LOCALAPPDATA", dir.path())
        .env("WI_VAULT", dir.path().join("vault"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn sleeping_pdf_tool_is_reported_per_page() {
    child("timeout", "10000");
}

#[test]
fn sleeping_tool_discovery_is_bounded_and_reported() {
    child("discovery", "10000");
}

#[test]
fn import_budget_preserves_sidecar_and_skips_remaining_pages() {
    child("budget", "100");
}

#[test]
fn pptx_total_expansion_is_bounded_across_members() {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut rels = String::from("<Relationships>");
    let mut presentation = String::from("<p:presentation xmlns:p=\"p\" xmlns:r=\"r\"><p:sldIdLst>");
    for i in 0..5 {
        rels.push_str(&format!(
            "<Relationship Id=\"r{i}\" Target=\"slides/s{i}.xml\"/>"
        ));
        presentation.push_str(&format!("<p:sldId r:id=\"r{i}\"/>"));
        writer
            .start_file(format!("ppt/slides/s{i}.xml"), opts)
            .unwrap();
        writer.write_all(b"<sld>").unwrap();
        // Tiny extracted text with 55 MiB of compressible, valid XML whitespace.
        for _ in 0..(55 * 128) {
            writer.write_all(&[b' '; 8192]).unwrap();
        }
        writer.write_all(b"<t>slide</t></sld>").unwrap();
    }
    rels.push_str("</Relationships>");
    presentation.push_str("</p:sldIdLst></p:presentation>");
    for (name, xml) in [
        ("ppt/_rels/presentation.xml.rels", rels),
        ("ppt/presentation.xml", presentation),
    ] {
        writer.start_file(name, opts).unwrap();
        writer.write_all(xml.as_bytes()).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = Vault::init_with(
        dir.path().join("vault"),
        OpenOptions {
            index_path: Some(dir.path().join("index.sqlite3")),
        },
    )
    .unwrap();
    std::fs::write(vault.root().join("large.pptx"), &bytes).unwrap();
    let report = import(
        &vault,
        "large.pptx",
        ImportOptions {
            ocr: Ocr::Off,
            bulk: false,
        },
    )
    .unwrap();
    assert_eq!(report.status, Status::Failed);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.contains("document expansion exceeds")),
        "{:?}",
        report.diagnostics
    );
}

#[test]
fn isolated_retry_child() {
    let Ok(step) = std::env::var("WI_DOCS_RETRY_STEP") else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os("WI_VAULT").unwrap());
    let options = OpenOptions {
        index_path: Some(root.with_extension("sqlite3")),
    };
    let vault = if step == "initial" {
        let (vault, _) = Vault::init_with(&root, options).unwrap();
        std::fs::write(vault.root().join("blank.pdf"), blank_pdf()).unwrap();
        vault
    } else {
        Vault::open_with(&root, options).unwrap()
    };
    let report = import(
        &vault,
        "blank.pdf",
        ImportOptions {
            ocr: Ocr::Off,
            bulk: false,
        },
    )
    .unwrap();
    match step.as_str() {
        "initial" => assert_eq!(report.status, Status::Partial),
        "failure" => assert_eq!(report.status, Status::Failed),
        "recover" => {
            assert_eq!(report.status, Status::NeedsOcr);
            assert!(!report.unchanged);
            assert!(
                !report
                    .diagnostics
                    .iter()
                    .any(|d| d.contains("import-budget"))
            );
        }
        _ => panic!("unknown retry step"),
    }
    std::fs::write(
        root.with_extension("report.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
}

#[test]
fn failed_retry_preserves_the_prior_raw_revision_and_can_retry_again() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    let invoke = |step: &str| {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "isolated_retry_child", "--nocapture"])
            .env("WI_DOCS_RETRY_STEP", step)
            .env("WI_VAULT", &root)
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
            .env("APPDATA", dir.path())
            .env("LOCALAPPDATA", dir.path())
            .env("PATH", dir.path())
            .env_remove("WI_IMPORT_BUDGET_MS")
            .env_remove("WI_TOOL_TIMEOUT_MS");
        if step == "initial" {
            child.env("WI_IMPORT_BUDGET_MS", "0");
        }
        if step == "failure" {
            child.env("TMPDIR", dir.path().join("missing-tmp"));
        }
        let out = child.output().unwrap();
        assert!(
            out.status.success(),
            "{step}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    invoke("initial");
    let report: wi_docs::ImportReport =
        serde_json::from_slice(&std::fs::read(root.with_extension("report.json")).unwrap())
            .unwrap();
    let raw = std::fs::read(root.join(&report.sidecar_path)).unwrap();
    let run = std::fs::read(root.join(&report.run_note_path)).unwrap();
    invoke("failure");
    assert!(
        std::fs::read(root.join(&report.sidecar_path)).unwrap() == raw,
        "sidecar changed during failed retry"
    );
    assert!(
        std::fs::read(root.join(&report.run_note_path)).unwrap() == run,
        "run changed during failed retry"
    );
    invoke("recover");
}

#[test]
fn isolated_tool_cache_child() {
    let Ok(root) = std::env::var("WI_DOCS_TOOL_CACHE_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let (vault, _) = Vault::init_with(
        root.join("vault"),
        OpenOptions {
            index_path: Some(root.join("index.sqlite3")),
        },
    )
    .unwrap();
    // Generated one-pixel PNG; the fake OCR emits a box within its dimensions.
    let png = [
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 0, 2, 0, 0, 5, 0,
        1, 165, 246, 69, 64, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    std::fs::write(vault.root().join("image.png"), png).unwrap();
    let options = ImportOptions {
        ocr: Ocr::Auto,
        bulk: false,
    };
    let head = || {
        let output = Command::new("git")
            .args(["-c", "core.autocrlf=false"])
            .env("PATH", std::env::var_os("WI_DOCS_TEST_PATH").unwrap())
            .arg("-C")
            .arg(vault.root())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    };
    let first = import(&vault, "image.png", options).unwrap();
    assert!(!first.unchanged);
    assert_eq!(first.status, Status::NeedsOcr);
    assert!(first.diagnostics.iter().any(|d| d.contains("unavailable")));
    vault.flush().unwrap();
    let first_head = head();
    if std::env::var("WI_DOCS_DISCOVERY_CASE").as_deref() != Ok("budget") {
        let cached = import(&vault, "image.png", options).unwrap();
        assert!(cached.unchanged);
        assert_eq!(cached.run_note_path, first.run_note_path);
        assert!(vault.flush().unwrap().commits.is_empty());
        assert_eq!(head(), first_head, "cached import created a commit");
    }

    let tool = root.join("bin/tesseract");
    let install = |version: &str| {
        std::fs::write(
            &tool,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n--version) echo 'fake-tesseract {version}';;\n--list-langs) echo eng;;\n*) printf 'level\\tpage_num\\tblock_num\\tpar_num\\tline_num\\tword_num\\tleft\\ttop\\twidth\\theight\\tconf\\ttext\\n5\\t1\\t1\\t1\\t1\\t1\\t0\\t0\\t1\\t1\\t99\\tHello\\n';;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    if let Ok(case) = std::env::var("WI_DOCS_DISCOVERY_CASE") {
        let script = match case.as_str() {
            "abnormal" => "#!/bin/sh\nexit 7\n",
            "empty" => "#!/bin/sh\nexit 0\n",
            "permission" => "#!/bin/sh\nexit 0\n",
            _ => "#!/bin/sh\nexec /bin/sleep 5\n",
        };
        std::fs::write(&tool, script).unwrap();
        std::fs::set_permissions(
            &tool,
            std::fs::Permissions::from_mode(if case == "permission" { 0o644 } else { 0o755 }),
        )
        .unwrap();
        let cause = match case.as_str() {
            "abnormal" => "abnormal exit",
            "empty" => "empty version output",
            "permission" => "Permission denied",
            _ => "tool-timeout",
        };
        for _ in 0..3 {
            let report = import(&vault, "image.png", options).unwrap();
            assert!(!report.unchanged, "unknown discovery was cached");
            assert!(
                report
                    .diagnostics
                    .iter()
                    .any(|d| d.contains("tesseract discovery:") && d.contains(cause)),
                "{:?}",
                report.diagnostics
            );
            let run = wi_docs::read_run(&vault.read(&report.run_note_path).unwrap().bytes).unwrap();
            assert!(run.diagnostics.iter().any(|d| d.contains(cause)));
            vault.flush().unwrap();
        }
        if case == "budget" {
            // Recovery is covered with a normal budget in the timeout case.
            return;
        }
    }
    // The short timeout only proves a hanging tool is cut off. A working
    // tool gets the same generous limit as the other cases, so a loaded
    // machine cannot turn the recovery below into a timeout.
    if std::env::var("WI_DOCS_DISCOVERY_CASE").as_deref() == Ok("timeout") {
        // SAFETY: this child process runs one test (`--exact`) on one
        // thread, and no import is running while the variable changes.
        unsafe { std::env::set_var("WI_TOOL_TIMEOUT_MS", "10000") };
    }
    install("1");
    let available = import(&vault, "image.png", options).unwrap();
    assert!(!available.unchanged);
    assert_eq!(available.status, Status::Complete);
    assert_ne!(available.run_note_path, first.run_note_path);
    assert!(available.diagnostics.is_empty());
    vault.flush().unwrap();
    let complete_head = head();
    assert!(import(&vault, "image.png", options).unwrap().unchanged);
    assert!(vault.flush().unwrap().commits.is_empty());
    assert_eq!(head(), complete_head);
    install("2");
    let upgraded = import(&vault, "image.png", options).unwrap();
    assert!(!upgraded.unchanged);
    assert_ne!(upgraded.run_note_path, available.run_note_path);
}

#[test]
fn missing_tool_is_cached_until_installed_or_upgraded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("bin")).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "isolated_tool_cache_child", "--nocapture"])
        .env("WI_DOCS_TOOL_CACHE_ROOT", dir.path())
        .env("WI_DOCS_TEST_PATH", std::env::var_os("PATH").unwrap())
        .env("PATH", dir.path().join("bin"))
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .env("APPDATA", dir.path())
        .env("LOCALAPPDATA", dir.path())
        .env_remove("WI_IMPORT_BUDGET_MS")
        .env_remove("WI_TOOL_TIMEOUT_MS")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn unknown_tool_discovery_retries_until_resolved() {
    for case in ["timeout", "budget", "abnormal", "empty", "permission"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "isolated_tool_cache_child", "--nocapture"])
            .env("WI_DOCS_TOOL_CACHE_ROOT", dir.path())
            .env("WI_DOCS_TEST_PATH", std::env::var_os("PATH").unwrap())
            .env("WI_DOCS_DISCOVERY_CASE", case)
            .env(
                "WI_TOOL_TIMEOUT_MS",
                if case == "timeout" { "300" } else { "10000" },
            )
            .env("PATH", dir.path().join("bin"))
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
            .env("APPDATA", dir.path())
            .env("LOCALAPPDATA", dir.path())
            .env(
                "WI_IMPORT_BUDGET_MS",
                if case == "budget" { "1" } else { "10000" },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
