use lopdf::{
    Document, Object, Stream,
    content::{Content, Operation},
    dictionary,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Cursor, Write},
};
use tempfile::TempDir;
use wi_docs::{
    ImportOptions, Ocr, Status, import, import_many,
    pins::{PagePin, RegionPin, resolve_page, resolve_region},
    read_run,
};
use wi_history::hash_bytes;
use wi_note::Note;
use wi_store::{OpenOptions, Query, SearchOptions, Vault};

fn setup() -> (TempDir, Vault) {
    let tmp = tempfile::tempdir().unwrap();
    let (vault, _) = Vault::init_with(
        tmp.path().join("vault"),
        OpenOptions {
            index_path: Some(tmp.path().join("cache.sqlite3")),
        },
    )
    .unwrap();
    (tmp, vault)
}
fn options() -> ImportOptions {
    ImportOptions {
        ocr: Ocr::Off,
        bulk: false,
    }
}
fn field(note: &Note, key: &str) -> Value {
    note.frontmatter().unwrap().get(key).unwrap()
}
fn zip(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, text) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(text.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}
fn pdf() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def /CMapName /Test def /CMapType 2 def 1 begincodespacerange <0000> <FFFF> endcodespacerange 2 beginbfchar <0001> <ACB0> <0002> <C81C> endbfchar endcmap CMapName currentdict /CMap defineresource pop end end";
    let cmap_id = doc.add_object(Stream::new(dictionary! {}, cmap.to_vec()));
    let cid = doc.add_object(dictionary!{"Type"=>"Font", "Subtype"=>"CIDFontType2", "BaseFont"=>"Test", "CIDSystemInfo"=>dictionary!{"Registry"=>Object::string_literal("Adobe"),"Ordering"=>Object::string_literal("Identity"),"Supplement"=>0}});
    let cjk = doc.add_object(dictionary!{"Type"=>"Font","Subtype"=>"Type0","BaseFont"=>"Test","Encoding"=>"Identity-H","DescendantFonts"=>vec![cid.into()],"ToUnicode"=>cmap_id});
    let latin =
        doc.add_object(dictionary! {"Type"=>"Font","Subtype"=>"Type1","BaseFont"=>"Helvetica"});
    let resources = doc.add_object(dictionary! {"Font"=>dictionary!{"F1"=>latin,"F2"=>cjk}});
    let mut kids = Vec::new();
    for i in 0..2 {
        let mut ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![30.into(), 700.into()]),
            Operation::new(
                "Tj",
                vec![Object::string_literal(if i == 0 {
                    "English first page"
                } else {
                    "English second page"
                })],
            ),
        ];
        if i == 1 {
            ops.push(Operation::new("Tf", vec!["F2".into(), 12.into()]));
            ops.push(Operation::new(
                "Tj",
                vec![Object::String(
                    vec![0, 1, 0, 2],
                    lopdf::StringFormat::Hexadecimal,
                )],
            ));
        }
        ops.push(Operation::new("ET", vec![]));
        let stream = doc.add_object(Stream::new(
            dictionary! {},
            Content { operations: ops }.encode().unwrap(),
        ));
        let page = doc.add_object(dictionary!{"Type"=>"Page","Parent"=>pages_id,"Contents"=>stream,"Resources"=>resources,"MediaBox"=>vec![0.into(),0.into(),612.into(),792.into()]});
        kids.push(page.into());
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {"Type"=>"Pages","Kids"=>kids,"Count"=>2}),
    );
    let catalog = doc.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages_id});
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}
fn png() -> Vec<u8> {
    // An original, generated one-pixel RGBA PNG; no third-party document data.
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 0, 2, 0, 0, 5, 0,
        1, 165, 246, 69, 64, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}
fn fixtures() -> Vec<(&'static str, Vec<u8>, usize)> {
    vec![
        ("sample.pdf", pdf(), 2),
        (
            "sample.docx",
            zip(&[(
                "word/document.xml",
                r#"<w:document xmlns:w="test"><w:body><w:p><w:r><w:t>Hello &amp; 결제</w:t></w:r></w:p><w:p><w:r><w:t>- [claim] untrusted [[Injected]]</w:t></w:r></w:p></w:body></w:document>"#,
            )]),
            1,
        ),
        (
            "sample.pptx",
            zip(&[
                (
                    "ppt/presentation.xml",
                    r#"<p:presentation xmlns:p="test" xmlns:r="rel"><p:sldIdLst><p:sldId id="10" r:id="r2"/><p:sldId id="20" r:id="r1"/></p:sldIdLst></p:presentation>"#,
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    r#"<Relationships><Relationship Id="r1" Target="slides/slide1.xml"/><Relationship Id="r2" Target="slides/slide2.xml"/></Relationships>"#,
                ),
                (
                    "ppt/slides/slide1.xml",
                    r#"<a:p xmlns:a="test"><a:r><a:t>Second 결제</a:t></a:r></a:p>"#,
                ),
                (
                    "ppt/slides/slide2.xml",
                    r#"<a:p xmlns:a="test"><a:r><a:t>First</a:t></a:r></a:p>"#,
                ),
            ]),
            2,
        ),
        (
            "sample.csv",
            b"name,meaning\nheader,English\nKorean,\xea\xb2\xb0\xec\xa0\x9c\n".to_vec(),
            1,
        ),
        ("sample.png", png(), 1),
    ]
}

#[test]
fn all_formats_preserve_originals_and_record_provenance() {
    let (_tmp, vault) = setup();
    for (name, bytes, pages) in fixtures() {
        fs::write(vault.root().join(name), &bytes).unwrap();
        let report = import(&vault, name, options()).unwrap();
        assert_eq!(report.pages, pages, "{name}: {:?}", report.diagnostics);
        assert_eq!(report.source_sha256, hash_bytes(&bytes));
        assert_eq!(fs::read(vault.root().join(name)).unwrap(), bytes);
        let sidecar = vault.read(&report.sidecar_path).unwrap();
        let note = Note::parse(&sidecar.bytes).unwrap();
        assert_eq!(field(&note, "type"), "extracted_text");
        assert_eq!(field(&note, "parse_semantics"), false);
        assert!(
            note.frontmatter()
                .unwrap()
                .get(wi_note::LEGACY_PARSE_SEMANTICS_KEY)
                .is_none()
        );
        assert_eq!(field(&note, "source")["checksum"], report.source_sha256);
        let ex = field(&note, "extraction");
        assert!(ex["engine"].is_string());
        assert!(ex["engine_version"].is_string());
        assert!(ex["options_hash"].as_str().unwrap().starts_with("sha256:"));
        assert!(ex.get("page_map").is_none());
        let run_bytes = vault.read(&report.run_note_path).unwrap().bytes;
        let run = Note::parse(&run_bytes).unwrap();
        assert_eq!(field(&run, "parse_semantics"), false);
        assert!(
            run.frontmatter()
                .unwrap()
                .get(wi_note::LEGACY_PARSE_SEMANTICS_KEY)
                .is_none()
        );
        assert_eq!(field(&run, "source"), field(&note, "source"));
        let map: wi_docs::pins::PageMap =
            serde_json::from_value(field(&run, "extraction")["page_map"].clone()).unwrap();
        map.validate(note.body()).unwrap();
        let payload = read_run(&run_bytes).unwrap();
        payload.utf8_page_map.validate(note.body()).unwrap();
        assert_eq!(field(&run, "output")["raw"]["checksum"], sidecar.checksum);
        if name.ends_with("pdf") {
            assert!(note.body().contains("결제"), "{}", note.body());
        }
        if name.ends_with("docx") {
            assert!(note.body().contains("Hello & 결제"), "{}", note.body());
        }
        if name.ends_with("pptx") {
            assert!(note.body().starts_with("First\nSecond"), "{}", note.body());
        }
        if name.ends_with("csv") {
            assert!(
                note.body()
                    .starts_with("| name | meaning |\n| --- | --- |\n")
            );
        }
    }
}

#[test]
fn searchable_with_only_source_provenance() {
    let (tmp, vault) = setup();
    let (_, bytes, _) = fixtures().remove(1);
    fs::write(vault.root().join("a.docx"), bytes).unwrap();
    let report = import(&vault, "a.docx", options()).unwrap();
    let note = Note::parse(&vault.read(&report.sidecar_path).unwrap().bytes).unwrap();
    assert!(note.observations().is_empty());
    assert!(note.relations().is_empty());
    assert!(note.wikilinks().is_empty());
    let query = Query {
        text: "결제".into(),
        ..Query::default()
    };
    let result = vault
        .search(&query, SearchOptions { catch_up: true })
        .unwrap();
    assert!(
        result
            .hits
            .iter()
            .any(|h| h.rel_path.to_str() == Some("a.docx.md"))
    );
    let db = rusqlite::Connection::open(tmp.path().join("cache.sqlite3")).unwrap();
    let observations: i64 = db
        .query_row("SELECT count(*) FROM observations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(observations, 0);
    let rows: Vec<(String, String, String)> = db
        .prepare("SELECT from_path,to_name,relation_type FROM relations")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        vec![("a.docx.md".into(), "a.docx".into(), "derived_from".into())]
    );
    drop(db);
    vault.reindex().unwrap();
    let db = rusqlite::Connection::open(tmp.path().join("cache.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM relations WHERE relation_type='derived_from'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn page_and_region_pins_validate_body_and_geometry() {
    let (_tmp, vault) = setup();
    fs::write(vault.root().join("a.pdf"), pdf()).unwrap();
    let report = import(&vault, "a.pdf", options()).unwrap();
    let payload = read_run(&vault.read(&report.run_note_path).unwrap().bytes).unwrap();
    let note = Note::parse(payload.sidecar.as_bytes()).unwrap();
    let body = note.body();
    let resolved = resolve_page(
        body,
        &payload.utf8_page_map,
        &PagePin {
            n: 2,
            label: Some("ii".into()),
        },
    )
    .unwrap();
    assert!(body[resolved.bytes.start..resolved.bytes.end].contains("second"));
    assert!(body[resolved.bytes.start..resolved.bytes.end].contains("결제"));
    let region = resolve_region(
        body,
        &payload.utf8_page_map,
        &payload.geometry,
        &RegionPin {
            page: 2,
            bbox: [1.0, 2.0, 100.0, 200.0],
        },
    )
    .unwrap();
    assert_eq!(region.region.unwrap().unit, "pdf_point");
    assert!(
        resolve_page(
            &format!("{body}x"),
            &payload.utf8_page_map,
            &PagePin { n: 2, label: None }
        )
        .is_err()
    );
    assert!(
        resolve_region(
            body,
            &payload.utf8_page_map,
            &payload.geometry,
            &RegionPin {
                page: 0,
                bbox: [0.0; 4]
            }
        )
        .is_err()
    );
    assert!(
        resolve_region(
            body,
            &payload.utf8_page_map,
            &payload.geometry,
            &RegionPin {
                page: 2,
                bbox: [0.0, 0.0, 1000.0, 1000.0]
            }
        )
        .is_err()
    );
    let unicode: wi_docs::pins::PageMap = serde_json::from_value(
        field(
            &Note::parse(&vault.read(&report.run_note_path).unwrap().bytes).unwrap(),
            "extraction",
        )["page_map"]
            .clone(),
    )
    .unwrap();
    assert!(payload.utf8_page_map.body_length > unicode.body_length);
    fs::write(vault.root().join("a.png"), png()).unwrap();
    let image = import(&vault, "a.png", options()).unwrap();
    let payload = read_run(&vault.read(&image.run_note_path).unwrap().bytes).unwrap();
    let note = Note::parse(payload.sidecar.as_bytes()).unwrap();
    assert_eq!(
        resolve_region(
            note.body(),
            &payload.utf8_page_map,
            &payload.geometry,
            &RegionPin {
                page: 1,
                bbox: [0.0, 0.0, 1.0, 1.0]
            }
        )
        .unwrap()
        .region
        .unwrap()
        .unit,
        "pixel"
    );
}

#[test]
fn local_no_model_ocr_off_import() {
    let (_tmp, vault) = setup();
    fs::write(vault.root().join("a.png"), png()).unwrap();
    let report = import(&vault, "a.png", options()).unwrap();
    assert_eq!(report.status, Status::NeedsOcr);
    fs::write(vault.root().join("a.csv"), "hello,offline\n").unwrap();
    assert_eq!(
        import(&vault, "a.csv", options()).unwrap().status,
        Status::Complete
    );
}

#[test]
fn reimport_noop_changed_source_and_hand_edits() {
    let (_tmp, vault) = setup();
    fs::write(vault.root().join("a.csv"), "first,value\n").unwrap();
    let first = import(&vault, "a.csv", options()).unwrap();
    vault.flush().unwrap();
    let generation = vault.generation().unwrap();
    let again = import(&vault, "a.csv", options()).unwrap();
    assert!(again.unchanged);
    assert_eq!(first.run_note_path, again.run_note_path);
    assert_eq!(generation, vault.generation().unwrap());
    fs::write(vault.root().join("a.csv"), "second,value\n").unwrap();
    let second = import(&vault, "a.csv", options()).unwrap();
    assert_ne!(first.run_note_path, second.run_note_path);
    assert_eq!(first.sidecar_path, second.sidecar_path);
    let before = fs::read(vault.root().join(&second.sidecar_path)).unwrap();
    let mut edit = before.clone();
    edit.extend_from_slice(b"Human enrichment\n");
    fs::write(vault.root().join(&second.sidecar_path), &edit).unwrap();
    assert!(matches!(
        import(&vault, "a.csv", options()),
        Err(wi_docs::Error::Conflict(_))
    ));
    assert_eq!(
        fs::read(vault.root().join(&second.sidecar_path)).unwrap(),
        edit
    );
    fs::write(vault.root().join(&second.sidecar_path), &before).unwrap();
    fs::write(vault.root().join("a.csv"), "first,value\n").unwrap();
    let restored = import(&vault, "a.csv", options()).unwrap();
    assert!(!restored.unchanged);
    assert_eq!(restored.run_note_path, first.run_note_path);
}

#[test]
fn bulk_import_is_one_commit_and_run_only_recovery() {
    let (_tmp, vault) = setup();
    for p in ["a.csv", "b.csv"] {
        fs::write(vault.root().join(p), "name,value\n").unwrap();
    }
    let results = import_many(&vault, &["a.csv".into(), "b.csv".into()], options()).unwrap();
    assert!(results.iter().all(Result::is_ok));
    let flush = vault.flush().unwrap();
    assert!(flush.blocked.is_none(), "{:?}", flush.blocked);
    assert_eq!(flush.commits.len(), 1);
    let report = results[0].as_ref().unwrap();
    fs::remove_file(vault.root().join(&report.sidecar_path)).unwrap();
    let repaired = import(&vault, "a.csv", options()).unwrap();
    assert!(!repaired.unchanged);
    assert_eq!(report.run_note_path, repaired.run_note_path);
}

#[test]
fn malformed_input_records_failure_without_overwriting_a_sidecar() {
    let (_tmp, vault) = setup();
    fs::write(vault.root().join("bad.pdf"), b"not a pdf").unwrap();
    let report = import(&vault, "bad.pdf", options()).unwrap();
    assert_eq!(report.status, Status::Failed);
    assert!(!vault.root().join(&report.sidecar_path).exists());
    assert_eq!(
        import(&vault, "bad.pdf", options()).unwrap().status,
        Status::Failed
    );
    let run = Note::parse(&vault.read(&report.run_note_path).unwrap().bytes).unwrap();
    assert_eq!(field(&run, "failure")["code"], "extraction_failed");
    assert_eq!(field(&run, "parse_semantics"), false);
    assert!(
        run.frontmatter()
            .unwrap()
            .get(wi_note::LEGACY_PARSE_SEMANTICS_KEY)
            .is_none()
    );
}

#[test]
fn uuid_derivation_matches_reference_vectors() {
    let source = uuid::Uuid::parse_str("12345678-1234-5678-1234-567812345678").unwrap();
    // Vectors are independently computed with Python uuid5/json.dumps.
    assert_eq!(
        wi_docs::document_id(source).to_string(),
        "dc9b8bfa-9116-55d4-8e87-5d90773a5309"
    );
    assert_eq!(
        wi_docs::run_id(
            source,
            &format!("sha256:{}", "a".repeat(64)),
            "csv",
            "1.4.0",
            &format!("sha256:{}", "b".repeat(64))
        )
        .to_string(),
        "45e384f2-af47-58c8-9fc9-5555ceb58ec4"
    );
}

#[test]
fn invalid_page_maps_and_region_bounds_are_refused() {
    let (body, mut map) =
        wi_docs::pins::PageMap::from_pages(&["결제".into(), "English".into()], true);
    map.pages[0].end = 1;
    map.pages[1].start = 1;
    assert!(map.validate(&body).is_err());
    let _ = json!({"body":body});
}

#[test]
fn bulk_closure_preserves_an_interleaved_move() {
    let (_tmp, vault) = setup();
    fs::write(vault.root().join("a.csv"), "hello,value\n").unwrap();
    vault
        .import_batch(|| {
            let report = import(&vault, "a.csv", options()).unwrap();
            let current = vault.read(&report.sidecar_path).unwrap();
            vault
                .move_note(wi_store::MoveRequest {
                    from: report.sidecar_path,
                    to: "moved.md".into(),
                    expected_checksum: Some(current.checksum),
                    op_id: None,
                    ingress: wi_store::Ingress::Cli,
                })
                .unwrap();
        })
        .unwrap();
    let flush = vault.flush().unwrap();
    assert!(flush.blocked.is_none(), "{:?}", flush.blocked);
    let head = wi_history::History::head(vault.history_vault())
        .unwrap()
        .unwrap();
    assert!(
        wi_history::History::blob_at(vault.history_vault(), &head, "a.csv.md")
            .unwrap()
            .is_none()
    );
    assert!(
        wi_history::History::blob_at(vault.history_vault(), &head, "moved.md")
            .unwrap()
            .is_some()
    );
}

#[test]
fn source_paths_remain_portable() {
    let (_tmp, vault) = setup();
    for path in ["CON.csv", "trailing .csv ", "bad:name.csv"] {
        fs::write(vault.root().join(path), "a,b\n").unwrap();
        assert!(import(&vault, path, options()).is_err(), "{path}");
    }
}

#[test]
fn doctor_advises_lfs_for_large_originals_without_configuration_changes() {
    let (_tmp, vault) = setup();
    let file = fs::File::create(vault.root().join("large.pdf")).unwrap();
    file.set_len(20 * 1024 * 1024 + 1).unwrap();
    let report = vault.doctor().unwrap();
    assert!(
        report
            .checks
            .iter()
            .any(|c| c.name == "large-attachments" && c.status == "warn")
    );
    assert!(!vault.root().join(".gitattributes").exists());
}

#[cfg(any(unix, windows))]
#[test]
fn import_refuses_symlink_sources_and_existing_sidecars() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file as symlink;
    let (tmp, vault) = setup();
    fs::write(tmp.path().join("outside.csv"), "a,b\n").unwrap();
    symlink(tmp.path().join("outside.csv"), vault.root().join("a.csv")).unwrap();
    assert!(import(&vault, "a.csv", options()).is_err());
    fs::remove_file(vault.root().join("a.csv")).unwrap();
    fs::write(vault.root().join("a.csv"), "a,b\n").unwrap();
    symlink(
        tmp.path().join("outside.csv"),
        vault.root().join("a.csv.md"),
    )
    .unwrap();
    assert!(import(&vault, "a.csv", options()).is_err());
    assert_eq!(
        fs::read_to_string(tmp.path().join("outside.csv")).unwrap(),
        "a,b\n"
    );
}

#[test]
fn integrated_pin_resolution_rejects_a_changed_original() {
    let (_tmp, vault) = setup();
    fs::write(vault.root().join("a.pdf"), pdf()).unwrap();
    let report = import(&vault, "a.pdf", options()).unwrap();
    let locator = wi_docs::Locator::Page(PagePin { n: 2, label: None });
    let pin = wi_docs::resolve_pin(&vault, &report.sidecar_path, &locator).unwrap();
    assert_eq!(pin.source_hash, report.source_sha256);
    assert_eq!(pin.resolved.page, 2);
    let region = wi_docs::Locator::Region(RegionPin {
        page: 2,
        bbox: [0.0, 0.0, 1.0, 1.0],
    });
    assert_eq!(
        wi_docs::resolve_pin(&vault, &report.sidecar_path, &region)
            .unwrap()
            .resolved
            .region
            .unwrap()
            .unit,
        "pdf_point"
    );
    fs::write(vault.root().join("a.pdf"), b"changed original bytes").unwrap();
    assert!(wi_docs::resolve_pin(&vault, &report.sidecar_path, &locator).is_err());
}

#[test]
fn pdf_nondefault_userunit_does_not_claim_point_geometry() {
    let (_tmp, vault) = setup();
    let mut doc = Document::load_mem(&pdf()).unwrap();
    for id in doc.get_pages().values() {
        doc.get_object_mut(*id)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("UserUnit", 2);
    }
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    fs::write(vault.root().join("scaled.pdf"), bytes).unwrap();
    let report = import(&vault, "scaled.pdf", options()).unwrap();
    let payload = read_run(&vault.read(&report.run_note_path).unwrap().bytes).unwrap();
    assert!(payload.geometry.iter().all(Option::is_none));
}

#[test]
fn body_normalization_keeps_empty_page_partition() {
    let (_tmp, vault) = setup();
    let bytes = zip(&[(
        "word/document.xml",
        r#"<w:document xmlns:w="test"><w:body><w:p/><w:p><w:r><w:t>\n</w:t></w:r></w:p><w:p><w:r><w:t>Body</w:t></w:r></w:p><w:p/></w:body></w:document>"#,
    )]);
    fs::write(vault.root().join("a.docx"), bytes).unwrap();
    let report = import(&vault, "a.docx", options()).unwrap();
    let note = Note::parse(&vault.read(&report.sidecar_path).unwrap().bytes).unwrap();
    assert!(!note.body().starts_with('\n'));
    assert!(note.body().ends_with('\n'));
    assert!(!note.body().ends_with("\n\n"));
}

#[test]
fn import_publishes_through_the_lifecycle_path_without_triggering_adoption() {
    let (_tmp, vault) = setup();
    assert!(vault.lifecycle_on());
    for p in ["a.csv", "b.csv"] {
        fs::write(vault.root().join(p), "name,value\n").unwrap();
    }
    let results = import_many(&vault, &["a.csv".into(), "b.csv".into()], options()).unwrap();
    assert!(results.iter().all(Result::is_ok));
    let flush = vault.flush().unwrap();
    assert_eq!(flush.commits.len(), 1);
    // Sidecars and runs are not memory notes: no gate, no work note, no
    // engine write inside the import commit.
    assert!(!vault.root().join("work").exists());
    let log = std::process::Command::new("git")
        .args(["-c", "core.autocrlf=false"])
        .arg("-C")
        .arg(vault.root())
        .args(["log", "-1", "--format=%B"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("Wi-Actor: import"), "{log}");
    assert!(!log.contains("Wi-Actor: engine"), "{log}");
}

#[test]
fn docx_headers_footers_and_table_rows_are_retained() {
    let (_tmp, vault) = setup();
    let bytes = zip(&[
        (
            "word/document.xml",
            r#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>r1c1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>결제</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>r2c1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>r2c2</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#,
        ),
        (
            "word/_rels/document.xml.rels",
            r#"<Relationships><Relationship Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/><Relationship Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/></Relationships>"#,
        ),
        (
            "word/header1.xml",
            r#"<w:hdr xmlns:w="w"><w:p><w:r><w:t>HEADER-TEXT-DOCX</w:t></w:r></w:p></w:hdr>"#,
        ),
        (
            "word/footer1.xml",
            r#"<w:ftr xmlns:w="w"><w:p><w:r><w:t>FOOTER-TEXT-DOCX</w:t></w:r></w:p></w:ftr>"#,
        ),
    ]);
    fs::write(vault.root().join("table.docx"), bytes).unwrap();
    let report = import(&vault, "table.docx", options()).unwrap();
    let note = Note::parse(&vault.read(&report.sidecar_path).unwrap().bytes).unwrap();
    assert!(
        note.body().contains("r1c1 | 결제\nr2c1 | r2c2\n"),
        "{}",
        note.body()
    );
    assert!(note.body().contains("## Header\nHEADER-TEXT-DOCX"));
    assert!(note.body().contains("## Footer\nFOOTER-TEXT-DOCX"));
}

#[test]
fn speaker_notes_follow_their_slide_relationship_not_filename() {
    let (_tmp, vault) = setup();
    let bytes = zip(&[
        (
            "ppt/presentation.xml",
            r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldId r:id="r2"/><p:sldId r:id="r1"/></p:presentation>"#,
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            r#"<Relationships><Relationship Id="r1" Target="slides/slide1.xml"/><Relationship Id="r2" Target="slides/slide2.xml"/></Relationships>"#,
        ),
        (
            "ppt/slides/slide1.xml",
            r#"<a:p xmlns:a="a"><a:t>Second slide</a:t></a:p>"#,
        ),
        (
            "ppt/slides/slide2.xml",
            r#"<a:p xmlns:a="a"><a:t>First slide</a:t></a:p>"#,
        ),
        (
            "ppt/slides/_rels/slide2.xml.rels",
            r#"<Relationships><Relationship Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide7.xml"/></Relationships>"#,
        ),
        (
            "ppt/notesSlides/notesSlide7.xml",
            r#"<a:p xmlns:a="a"><a:t>NOTES-for-first 한국어</a:t></a:p>"#,
        ),
    ]);
    fs::write(vault.root().join("notes.pptx"), bytes).unwrap();
    let report = import(&vault, "notes.pptx", options()).unwrap();
    let payload = read_run(&vault.read(&report.run_note_path).unwrap().bytes).unwrap();
    let note = Note::parse(payload.sidecar.as_bytes()).unwrap();
    let page = resolve_page(
        note.body(),
        &payload.utf8_page_map,
        &PagePin { n: 1, label: None },
    )
    .unwrap();
    assert_eq!(
        &note.body()[page.bytes.start..page.bytes.end],
        "First slide\n\n## Speaker notes\nNOTES-for-first 한국어\n"
    );
    assert!(!note.body()[page.bytes.end..].contains("NOTES-"));
}

#[test]
fn csv_tables_keep_records_with_escaped_cells_and_wide_tsv() {
    let (_tmp, vault) = setup();
    fs::write(
        vault.root().join("a.csv"),
        "\u{feff}name,value\n\"한국어\n줄바꿈\",\"a|b\\n\"\n",
    )
    .unwrap();
    let report = import(&vault, "a.csv", options()).unwrap();
    let note = Note::parse(&vault.read(&report.sidecar_path).unwrap().bytes).unwrap();
    assert_eq!(
        note.body(),
        "| name | value |\n| --- | --- |\n| 한국어\\n줄바꿈 | a\\|b\\\\n |\n"
    );
    let wide = (0..51)
        .map(|i| format!("col{i}"))
        .collect::<Vec<_>>()
        .join(",");
    fs::write(vault.root().join("wide.csv"), format!("{wide}\n{wide}\n")).unwrap();
    let report = import(&vault, "wide.csv", options()).unwrap();
    let note = Note::parse(&vault.read(&report.sidecar_path).unwrap().bytes).unwrap();
    assert_eq!(note.body().lines().count(), 2);
    assert!(note.body().starts_with("col0\tcol1\t"));
}

fn with_note_bound(tmp: &TempDir, vault: Vault, bound: usize) -> Vault {
    let config = vault.root().join(".wiki-island/config.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!("\nmax_note_bytes = {bound}\n"));
    fs::write(config, text).unwrap();
    let root = vault.root().to_path_buf();
    drop(vault);
    Vault::open_with(
        root,
        OpenOptions {
            index_path: Some(tmp.path().join("cache.sqlite3")),
        },
    )
    .unwrap()
}

#[test]
fn configured_bound_keeps_whole_pdf_prefix_and_both_notes_searchable() {
    #[cfg(windows)]
    if std::env::var_os("WI_NOTE_BOUND_TEST_CHILD").is_none() {
        // This case tests the embedded PDF extractor and size bounds,
        // independently of optional tools installed on the host.
        let tools = TempDir::new().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "configured_bound_keeps_whole_pdf_prefix_and_both_notes_searchable",
                "--nocapture",
            ])
            .env("WI_NOTE_BOUND_TEST_CHILD", "1")
            .env("PATH", tools.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    let (tmp, vault) = setup();
    let vault = with_note_bound(&tmp, vault, 12_000);
    let mut doc = Document::load_mem(&pdf()).unwrap();
    for (n, id) in doc.get_pages() {
        let content_id = doc
            .get_object(id)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .as_reference()
            .unwrap();
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new(
                    "Tj",
                    vec![Object::string_literal(if n == 1 {
                        format!("prefixneedle {}", "first ".repeat(300))
                    } else {
                        "tailneedle ".repeat(2_000)
                    })],
                ),
                Operation::new("ET", vec![]),
            ],
        }
        .encode()
        .unwrap();
        doc.objects.insert(
            content_id,
            Object::Stream(Stream::new(dictionary! {}, content)),
        );
    }
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    fs::write(vault.root().join("large.pdf"), bytes).unwrap();
    let report = import(&vault, "large.pdf", options()).unwrap();
    assert_eq!(report.status, Status::Partial);
    assert!(report.diagnostics.iter().any(|s| s.contains("note bound")));
    let run = vault.read(&report.run_note_path).unwrap();
    let sidecar = vault.read(&report.sidecar_path).unwrap();
    assert!(run.bytes.len() <= 12_000 && sidecar.bytes.len() <= 12_000);
    let payload = read_run(&run.bytes).unwrap();
    assert_eq!(payload.pages_not_indexed, [2]);
    assert_eq!(payload.utf8_page_map.pages.len(), 2);
    let query = Query {
        text: "prefixneedle".into(),
        ..Query::default()
    };
    let hits = vault
        .search(&query, SearchOptions { catch_up: true })
        .unwrap();
    assert!(
        hits.hits
            .iter()
            .any(|h| h.rel_path.to_str() == Some("large.pdf.md"))
    );
    assert!(
        !hits
            .diagnostics
            .iter()
            .any(|s| s.contains("exceeds configured")),
        "{:?}",
        hits.diagnostics
    );
    assert!(
        wi_docs::resolve_pin(
            &vault,
            &report.sidecar_path,
            &wi_docs::Locator::Page(PagePin { n: 2, label: None })
        )
        .is_err()
    );
    let repeated = import(&vault, "large.pdf", options()).unwrap();
    assert!(repeated.unchanged, "{repeated:?}");
}

#[test]
fn large_csv_is_honestly_partial_and_source_cap_refuses_before_loading() {
    let (tmp, vault) = setup();
    let vault = with_note_bound(&tmp, vault, 12_000);
    fs::write(
        vault.root().join("large.csv"),
        "longrow,value\n".repeat(20_000),
    )
    .unwrap();
    let report = import(&vault, "large.csv", options()).unwrap();
    assert_eq!(report.status, Status::Partial);
    let run = vault.read(&report.run_note_path).unwrap();
    assert!(run.bytes.len() <= 12_000);
    assert_eq!(read_run(&run.bytes).unwrap().pages_not_indexed, [1]);
    let file = fs::File::create(vault.root().join("huge.pdf")).unwrap();
    file.set_len(wi_docs::MAX_SOURCE_BYTES + 1).unwrap();
    let err = import(&vault, "huge.pdf", options())
        .unwrap_err()
        .to_string();
    assert!(err.contains("safety cap"), "{err}");
    assert!(!vault.root().join("huge.pdf.md").exists());
}

#[test]
fn metadata_that_cannot_fit_is_refused_without_publishing_derived_notes() {
    let (tmp, vault) = setup();
    let vault = with_note_bound(&tmp, vault, 100);
    fs::write(vault.root().join("tiny.csv"), "a,b\n").unwrap();
    let err = import(&vault, "tiny.csv", options())
        .unwrap_err()
        .to_string();
    assert!(err.contains("metadata alone"));
    assert!(!vault.root().join("tiny.csv.md").exists());
    assert!(!vault.root().join("document-ingestion-runs").exists());
}

fn local_tool(name: &str, flag: &str) -> bool {
    std::process::Command::new(name)
        .arg(flag)
        .output()
        .is_ok_and(|out| out.status.success())
}

#[test]
fn korean_cupsfilter_pdf_uses_poppler_fallback_when_installed() {
    if !local_tool("pdftotext", "-v") || !std::path::Path::new("/usr/sbin/cupsfilter").exists() {
        eprintln!("skipping Korean PDF fallback: cupsfilter or pdftotext unavailable");
        return;
    }
    let (_tmp, vault) = setup();
    let text = vault.root().join("korean.txt");
    fs::write(&text, "한국어 결제 정책\n").unwrap();
    let out = std::process::Command::new("/usr/sbin/cupsfilter")
        .arg(&text)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::write(vault.root().join("korean.pdf"), out.stdout).unwrap();
    let report = import(&vault, "korean.pdf", options()).unwrap();
    assert_eq!(report.status, Status::Complete, "{:?}", report.diagnostics);
    let run = read_run(&vault.read(&report.run_note_path).unwrap().bytes).unwrap();
    let note = Note::parse(run.sidecar.as_bytes()).unwrap();
    assert!(
        note.body().contains("한국어") && note.body().contains("결제"),
        "{}",
        note.body()
    );
    assert_eq!(run.page_extractors[0].engine, "pdftotext");
    assert!(!run.page_extractors[0].version.is_empty());
    // The sidecar itself records the fallback extractor.
    let tags = note.frontmatter().unwrap().get("tags").unwrap();
    assert_eq!(
        tags,
        serde_json::json!(["document", "generated", "extractor/pdftotext"])
    );
}

fn scanned_pdf() -> Vec<u8> {
    // Original bitmap font HELLO, enlarged with white margins for OCR.
    let glyphs = [
        [
            "10001", "10001", "10001", "11111", "10001", "10001", "10001",
        ],
        [
            "11111", "10000", "10000", "11110", "10000", "10000", "11111",
        ],
        [
            "10000", "10000", "10000", "10000", "10000", "10000", "11111",
        ],
        [
            "10000", "10000", "10000", "10000", "10000", "10000", "11111",
        ],
        [
            "01110", "10001", "10001", "10001", "10001", "10001", "01110",
        ],
    ];
    let (width, height, scale) = (800usize, 300usize, 5usize);
    let mut pixels = vec![255u8; width * height];
    for (g, glyph) in glyphs.iter().enumerate() {
        for (y, row) in glyph.iter().enumerate() {
            for (x, bit) in row.bytes().enumerate() {
                if bit == b'1' {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            pixels[(85 + y * scale + dy) * width
                                + 100
                                + g * 6 * scale
                                + x * scale
                                + dx] = 0;
                        }
                    }
                }
            }
        }
    }
    let mut doc = Document::with_version("1.5");
    let pages = doc.new_object_id();
    let image = doc.add_object(Stream::new(dictionary! {"Type"=>"XObject", "Subtype"=>"Image", "Width"=>width as i64, "Height"=>height as i64, "ColorSpace"=>"DeviceGray", "BitsPerComponent"=>8}, pixels));
    let content = doc.add_object(Stream::new(
        dictionary! {},
        b"q 800 0 0 300 0 0 cm /Scan Do Q".to_vec(),
    ));
    let page = doc.add_object(dictionary!{"Type"=>"Page", "Parent"=>pages, "MediaBox"=>vec![0.into(),0.into(),800.into(),300.into()], "Contents"=>content, "Resources"=>dictionary!{"XObject"=>dictionary!{"Scan"=>image}}});
    doc.objects.insert(
        pages,
        Object::Dictionary(dictionary! {"Type"=>"Pages", "Kids"=>vec![page.into()], "Count"=>1}),
    );
    let root = doc.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
    doc.trailer.set("Root", root);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn scanned_pdf_ocr_uses_local_rasterization_when_installed() {
    if !local_tool("pdftoppm", "-v") || !local_tool("tesseract", "--version") {
        eprintln!("skipping scanned PDF OCR: pdftoppm or tesseract unavailable");
        return;
    }
    let (_tmp, vault) = setup();
    let bytes = scanned_pdf();
    fs::write(vault.root().join("scan.pdf"), &bytes).unwrap();
    let off = import(&vault, "scan.pdf", options()).unwrap();
    assert_eq!(off.status, Status::NeedsOcr);
    let report = import(
        &vault,
        "scan.pdf",
        ImportOptions {
            ocr: Ocr::Required,
            bulk: false,
        },
    )
    .unwrap();
    assert_eq!(report.status, Status::Complete, "{:?}", report.diagnostics);
    let payload = read_run(&vault.read(&report.run_note_path).unwrap().bytes).unwrap();
    let note = Note::parse(payload.sidecar.as_bytes()).unwrap();
    // The block-font final O may be read as U; this smoke test verifies
    // the raster/OCR path rather than claiming exact OCR quality.
    assert!(note.body().contains("HELL"), "{}", note.body());
    assert_eq!(payload.page_extractors[0].engine, "pdftoppm+tesseract");
    assert!(payload.page_extractors[0].version.contains("tesseract"));
    let tags = note.frontmatter().unwrap().get("tags").unwrap();
    assert_eq!(
        tags,
        serde_json::json!([
            "document",
            "generated",
            "extractor/pdftoppm-tesseract",
            "ocr"
        ])
    );
    assert_eq!(fs::read(vault.root().join("scan.pdf")).unwrap(), bytes);
    assert_ne!(off.run_note_path, report.run_note_path);
}

#[test]
fn import_reloads_index_bound_for_a_long_lived_vault() {
    let (_tmp, vault) = setup();
    let path = vault.root().join(".wiki-island/config.toml");
    let mut config = fs::read_to_string(&path).unwrap();
    config.push_str("\nmax_note_bytes = 12000\n");
    fs::write(&path, config).unwrap();
    assert_eq!(vault.config().max_note_bytes(), 8 * 1024 * 1024);
    fs::write(vault.root().join("live.csv"), "row,value\n".repeat(20_000)).unwrap();
    let report = import(&vault, "live.csv", options()).unwrap();
    assert_eq!(report.status, Status::Partial);
    assert!(vault.read(&report.run_note_path).unwrap().bytes.len() <= 12_000);
}

fn git(vault: &Vault, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "core.autocrlf=false"])
        .arg("-C")
        .arg(vault.root())
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn derived_from_resolves_to_the_original_binary_not_the_sidecar() {
    let (tmp, vault) = setup();
    fs::write(vault.root().join("a.csv"), "name,value\n").unwrap();
    let report = import(&vault, "a.csv", options()).unwrap();
    // A note whose title equals the binary name must not capture the edge.
    vault
        .write(wi_store::WriteRequest {
            path: "other.md".into(),
            content: b"---\ntitle: a.csv\n---\nbody\n".to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress: wi_store::Ingress::Cli,
        })
        .unwrap();
    let edge = |db: &rusqlite::Connection| -> Option<String> {
        db.query_row(
            "SELECT to_path FROM relations WHERE relation_type='derived_from'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    let query = Query {
        text: "body".into(),
        ..Query::default()
    };
    vault
        .search(&query, SearchOptions { catch_up: true })
        .unwrap();
    let db = rusqlite::Connection::open(tmp.path().join("cache.sqlite3")).unwrap();
    assert_eq!(edge(&db).as_deref(), Some("a.csv"));
    drop(db);
    vault.reindex().unwrap();
    let db = rusqlite::Connection::open(tmp.path().join("cache.sqlite3")).unwrap();
    assert_eq!(edge(&db).as_deref(), Some("a.csv"));
    drop(db);

    // Context names the original file without treating it as a note node.
    let db = rusqlite::Connection::open(tmp.path().join("cache.sqlite3")).unwrap();
    let permalink: String = db
        .query_row(
            "SELECT permalink FROM notes WHERE path=?1",
            [report.sidecar_path.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    drop(db);
    let context = vault.context(&permalink, 2).unwrap();
    assert!(context.diagnostics.is_empty(), "{:?}", context.diagnostics);
    assert_eq!(context.hits.len(), 1);
    let edges: Vec<_> = context
        .edges
        .iter()
        .map(|e| {
            (
                e.from_path.to_string_lossy().into_owned(),
                e.to_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                e.relation_type.clone(),
            )
        })
        .collect();
    assert_eq!(
        edges,
        vec![(
            "a.csv.md".into(),
            Some("a.csv".into()),
            "derived_from".into()
        )]
    );
}

#[test]
fn import_commit_subject_counts_changed_files_not_records() {
    let (_tmp, vault) = setup();
    let names = ["a.csv", "b.csv", "c.csv"];
    for p in names {
        fs::write(vault.root().join(p), "name,value\n").unwrap();
    }
    let paths: Vec<String> = names.iter().map(|p| (*p).into()).collect();
    let results = import_many(&vault, &paths, options()).unwrap();
    assert!(results.iter().all(Result::is_ok));
    let flush = vault.flush().unwrap();
    assert_eq!(flush.commits.len(), 1);
    let changed = git(
        &vault,
        &["show", "--name-only", "--format=", "--no-renames", "HEAD"],
    )
    .lines()
    .filter(|l| !l.is_empty())
    .count();
    let subject = git(&vault, &["log", "-1", "--format=%s"]);
    assert!(
        subject.trim_end().ends_with(&format!("({changed} files)")),
        "{subject} vs {changed} changed files"
    );
}

#[cfg(windows)]
#[test]
fn native_separators_import_the_same_document_and_keep_portable_sidecar_paths() {
    let (_temp, vault) = setup();
    fs::create_dir_all(vault.root().join("docs")).unwrap();
    fs::write(vault.root().join("docs/table.csv"), "a,b\n").unwrap();
    let first = import(&vault, r"docs\table.csv", options()).unwrap();
    assert_eq!(first.sidecar_path, "docs/table.csv.md");
    let again = import(&vault, "docs/table.csv", options()).unwrap();
    assert!(again.unchanged);
}
