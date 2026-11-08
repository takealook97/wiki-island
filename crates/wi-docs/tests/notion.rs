use serde_json::{Value, json};
use std::{
    fs,
    io::{Cursor, Write},
};
use tempfile::TempDir;
use wi_docs::{ImportOptions, Status, import, import_many};
use wi_note::Note;
use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

const HOME: &str = "Home 11111111111111111111111111111111";
const OTHER: &str = "Other Page 22222222222222222222222222222222.md";
const DB: &str = "Tasks 33333333333333333333333333333333";
const ROW: &str = "First 44444444444444444444444444444444.md";

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
fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}
fn write(vault: &Vault, path: &str, bytes: impl AsRef<[u8]>) {
    fs::create_dir_all(vault.root().join(path).parent().unwrap()).unwrap();
    fs::write(vault.root().join(path), bytes).unwrap();
}
fn read(vault: &Vault, path: &str) -> Note {
    Note::parse(&vault.read(path).unwrap().bytes).unwrap()
}
fn field(note: &Note, key: &str) -> Value {
    note.frontmatter().unwrap().get(key).unwrap()
}
fn opts() -> ImportOptions {
    ImportOptions::default()
}

#[test]
fn zip_pages_hierarchy_links_and_idempotent_updates() {
    let (_tmp, vault) = setup();
    let home = format!("{HOME}.md");
    let child = format!("{HOME}/Child aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.md");
    let link = format!(
        "# Home\n\n[Other](Other%20Page%2022222222222222222222222222222222.md)\n\n[Child]({}/Child%20aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.md)\n",
        HOME.replace(' ', "%20")
    );
    let bytes = zip(&[
        (&home, link.as_bytes()),
        (OTHER, b"# Other\n\nDurable content."),
        (&child, b"# Child\n"),
    ]);
    write(&vault, "export.zip", &bytes);
    let report = import(&vault, "export.zip", opts()).unwrap();
    assert_eq!(report.status, Status::Complete);
    assert_eq!(report.pages, 3);
    vault.reindex().unwrap();
    let db = rusqlite::Connection::open(vault.index_database().unwrap().unwrap()).unwrap();
    let resolved: i64 = db
        .query_row(
            "SELECT count(*) FROM relations WHERE from_path=?1 AND to_path IS NOT NULL",
            [format!("{}/Home.md", report.sidecar_path)],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(resolved, 2);
    drop(db);
    let output = format!("{}/Home.md", report.sidecar_path);
    let note = read(&vault, &output);
    assert!(
        note.body()
            .contains(&format!("[[{}/Other Page.md|Other]]", report.sidecar_path))
    );
    assert!(
        note.body()
            .contains(&format!("[[{}/Home/Child.md|Child]]", report.sidecar_path))
    );
    assert_eq!(field(&note, "type"), "note");
    assert!(note.frontmatter().unwrap().get("source").is_none());
    assert_eq!(field(&note, "origin")["kind"], "file");
    assert!(note.frontmatter().unwrap().get("adoption").is_none());
    let manifest = read(&vault, &report.run_note_path);
    assert!(manifest.body().contains("Other Page"));
    vault.flush().unwrap();
    let mark = vault.publication_mark().unwrap();
    assert!(import(&vault, "export.zip", opts()).unwrap().unchanged);
    assert_eq!(vault.publication_mark().unwrap(), mark);
    let changed = zip(&[
        (&home, link.as_bytes()),
        (OTHER, b"# Other\n\nUpdated durable content."),
        (&child, b"# Child\n"),
    ]);
    write(&vault, "export.zip", changed);
    let home_before = vault.read(&output).unwrap().bytes;
    let history_before = vault.history(&output).unwrap().len();
    assert!(!import(&vault, "export.zip", opts()).unwrap().unchanged);
    assert_eq!(vault.read(&output).unwrap().bytes, home_before);
    vault.flush().unwrap();
    assert_eq!(vault.history(&output).unwrap().len(), history_before);
    assert!(
        read(&vault, &format!("{}/Other Page.md", report.sidecar_path))
            .body()
            .contains("Updated")
    );
    assert_eq!(
        fs::read(vault.root().join("export.zip")).unwrap(),
        zip(&[
            (&home, link.as_bytes()),
            (OTHER, b"# Other\n\nUpdated durable content."),
            (&child, b"# Child\n")
        ])
    );
}

#[test]
fn folder_csv_rows_merge_page_body_and_preserve_properties_as_data() {
    let (_tmp, vault) = setup();
    let csv = format!("export/{DB}.csv");
    write(
        &vault,
        &csv,
        "Name,type,Details\r\nFirst,rule,\"line one\nline two\"\r\nSecond,preference,\"a,b\"\r\n",
    );
    write(
        &vault,
        &format!("export/{DB}/{ROW}"),
        "# First\n\nPage body.",
    );
    assert!(wi_docs::notion::is_export(vault.root(), "export"));
    let report = import_many(&vault, &["export".into()], opts())
        .unwrap()
        .remove(0)
        .unwrap();
    assert_eq!(report.pages, 3);
    let first = read(&vault, &format!("{}/Tasks/First.md", report.sidecar_path));
    assert_eq!(field(&first, "type"), "note");
    assert_eq!(field(&first, "notion_type"), "rule");
    assert_eq!(field(&first, "Details"), "line one\nline two");
    assert!(first.body().contains("Page body."));
    let second = read(&vault, &format!("{}/Tasks/Second.md", report.sidecar_path));
    assert_eq!(field(&second, "Details"), "a,b");
    assert!(
        read(&vault, &format!("{}/Tasks.md", report.sidecar_path))
            .body()
            .contains("Tasks/First")
    );
    assert!(import(&vault, "export", opts()).unwrap().unchanged);
}

#[test]
fn duplicate_titles_have_stable_paths_when_another_page_is_added() {
    let (_tmp, vault) = setup();
    let first = "Same bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.md";
    let second = "Same cccccccccccccccccccccccccccccccc.md";
    write(
        &vault,
        "export.zip",
        zip(&[(first, b"first"), (second, b"second")]),
    );
    let report = import(&vault, "export.zip", opts()).unwrap();
    let original = vault
        .read(&format!("{}/Same.md", report.sidecar_path))
        .unwrap()
        .bytes;
    assert!(
        read(&vault, &format!("{}/Same (2).md", report.sidecar_path))
            .body()
            .contains("second")
    );
    write(
        &vault,
        "export.zip",
        zip(&[
            ("Same aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.md", b"new"),
            (first, b"first"),
            (second, b"second"),
        ]),
    );
    import(&vault, "export.zip", opts()).unwrap();
    assert_eq!(
        vault
            .read(&format!("{}/Same.md", report.sidecar_path))
            .unwrap()
            .bytes,
        original
    );
    assert!(
        read(&vault, &format!("{}/Same (3).md", report.sidecar_path))
            .body()
            .contains("new")
    );
}

#[test]
fn hand_edits_and_foreign_files_are_not_overwritten() {
    let (_tmp, vault) = setup();
    let name = format!("{HOME}.md");
    write(&vault, "export.zip", zip(&[(&name, b"old")]));
    let report = import(&vault, "export.zip", opts()).unwrap();
    let path = format!("{}/Home.md", report.sidecar_path);
    let before = vault.read(&path).unwrap();
    let mut edited = before.bytes.clone();
    edited.extend_from_slice(b"\nHuman edit.");
    vault
        .write(WriteRequest {
            path: path.clone(),
            content: edited.clone(),
            expected_checksum: Some(before.checksum),
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
    write(&vault, "export.zip", zip(&[(&name, b"new")]));
    assert!(matches!(
        import(&vault, "export.zip", opts()),
        Err(wi_docs::Error::Conflict(_))
    ));
    assert_eq!(vault.read(&path).unwrap().bytes, edited);
}

#[test]
fn zip_slip_absolute_duplicate_and_symlink_members_are_rejected_before_writes() {
    let (_tmp, vault) = setup();
    let home = format!("{HOME}.md");
    for bad in [
        "../outside.md",
        "/outside.md",
        "a/../../outside.md",
        "a\\outside.md",
        "a//outside.md",
    ] {
        write(&vault, "bad.zip", zip(&[(&home, b"safe"), (bad, b"bad")]));
        assert!(import(&vault, "bad.zip", opts()).is_err(), "{bad}");
    }
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(&home, zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"safe").unwrap();
    writer
        .add_symlink(
            "escape",
            "../../outside",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    write(&vault, "bad.zip", writer.finish().unwrap().into_inner());
    assert!(import(&vault, "bad.zip", opts()).is_err());
    assert!(!vault.root().join("Notion").exists());
}

#[test]
fn code_examples_and_external_links_are_preserved() {
    let (_tmp, vault) = setup();
    let home = format!("{HOME}.md");
    let link = format!("[Other]({OTHER})");
    let body = format!(
        "# Home\n\n`{link}`\n\n```md\n{link}\n```\n\n    {link}\n\n`multiline\n{link}`\n\n<!--\n{link}\n-->\n\n[external](https://example.com/a.md)\n\n{link}\n"
    );
    write(
        &vault,
        "export.zip",
        zip(&[(&home, body.as_bytes()), (OTHER, b"# Other")]),
    );
    let report = import(&vault, "export.zip", opts()).unwrap();
    let result = read(&vault, &format!("{}/Home.md", report.sidecar_path));
    assert!(result.body().contains(&format!("`{link}`")));
    assert!(result.body().contains(&format!("```md\n{link}\n```")));
    assert!(
        result
            .body()
            .contains("[external](https://example.com/a.md)")
    );
    assert_eq!(result.body().matches(&link).count(), 5);
    assert_eq!(result.wikilinks().len(), 1);
}

#[test]
fn attachments_are_copied_beside_the_pages_and_kept_in_step() {
    let (_tmp, vault) = setup();
    let home = format!("{HOME}.md");
    write(
        &vault,
        &format!("export/{home}"),
        "# Home\n\n![picture](asset.png)",
    );
    write(&vault, "export/asset.png", b"original binary asset");
    let report = import(&vault, "export", opts()).unwrap();
    assert_eq!(report.status, Status::Complete, "{:?}", report.diagnostics);
    let copy = format!("{}/asset.png", report.sidecar_path);
    let note = read(&vault, &format!("{}/Home.md", report.sidecar_path));
    assert!(note.body().contains(&format!("![[{copy}|picture]]")));
    assert_eq!(
        fs::read(vault.root().join(&copy)).unwrap(),
        b"original binary asset"
    );
    assert_eq!(
        fs::read(vault.root().join("export/asset.png")).unwrap(),
        b"original binary asset"
    );
    vault.flush().unwrap();
    assert!(vault.history(&copy).unwrap().len() == 1);

    // A ZIP export carries its attachments too; a second import is a no-op.
    write(
        &vault,
        "export.zip",
        zip(&[(&home, b"![picture](asset.png)"), ("asset.png", b"data")]),
    );
    let report = import(&vault, "export.zip", opts()).unwrap();
    assert_eq!(report.status, Status::Complete);
    let zipped = format!("{}/asset.png", report.sidecar_path);
    assert_eq!(fs::read(vault.root().join(&zipped)).unwrap(), b"data");
    let before = vault.publication_mark().unwrap();
    assert!(import(&vault, "export.zip", opts()).unwrap().unchanged);
    assert_eq!(vault.publication_mark().unwrap(), before);

    // Newer source bytes replace the copy the last import wrote.
    write(
        &vault,
        "export.zip",
        zip(&[(&home, b"![picture](asset.png)"), ("asset.png", b"newer")]),
    );
    import(&vault, "export.zip", opts()).unwrap();
    assert_eq!(fs::read(vault.root().join(&zipped)).unwrap(), b"newer");

    // A copy changed in the vault since is never overwritten.
    write(&vault, &zipped, b"edited here");
    write(
        &vault,
        "export.zip",
        zip(&[(&home, b"![picture](asset.png)"), ("asset.png", b"newest")]),
    );
    assert!(import(&vault, "export.zip", opts()).is_err());
    assert_eq!(
        fs::read(vault.root().join(&zipped)).unwrap(),
        b"edited here"
    );
}

#[test]
fn attachment_writes_stay_inside_the_vault_and_need_an_import() {
    let (_tmp, vault) = setup();
    let write = |path: &str, ingress| vault.write_attachment(path, b"x".to_vec(), None, ingress);
    assert!(write("files/a.png", Ingress::Mcp).is_err());
    for path in [
        "../out.png",
        ".git/x.png",
        ".wiki-island/x.png",
        "note.md",
        ".obsidian/x.png",
    ] {
        assert!(write(path, Ingress::Import).is_err(), "{path}");
    }
    let saved = write("files/a.png", Ingress::Import).unwrap();
    assert!(write("files/a.png", Ingress::Import).is_err());
    assert!(
        vault
            .write_attachment(
                "files/a.png",
                b"y".to_vec(),
                Some("0".repeat(64)),
                Ingress::Import
            )
            .is_err()
    );
    vault
        .write_attachment(
            "files/a.png",
            b"y".to_vec(),
            Some(saved.checksum),
            Ingress::Import,
        )
        .unwrap();
    assert_eq!(fs::read(vault.root().join("files/a.png")).unwrap(), b"y");
}

#[test]
fn configured_note_limit_and_invalid_csv_fail_before_publication() {
    let (_tmp, vault) = setup();
    let home = format!("{HOME}.md");
    write(
        &vault,
        "export.zip",
        zip(&[
            (&home, b"body"),
            (&format!("{DB}.csv"), b"Name,Name\nOne,Two\n"),
        ]),
    );
    let before = vault.publication_mark().unwrap();
    assert!(import(&vault, "export.zip", opts()).is_err());
    assert_eq!(vault.publication_mark().unwrap(), before);
    let large = vec![b'x'; 8 * 1024 * 1024 + 1];
    write(&vault, "export.zip", zip(&[(&home, &large)]));
    assert!(import(&vault, "export.zip", opts()).is_err());
    assert_eq!(vault.publication_mark().unwrap(), before);
}

#[cfg(any(unix, windows))]
#[test]
fn folder_symlinks_are_not_followed() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file as symlink;
    let (tmp, vault) = setup();
    write(&vault, &format!("export/{HOME}.md"), b"safe");
    fs::write(tmp.path().join("outside.md"), b"private").unwrap();
    symlink(
        tmp.path().join("outside.md"),
        vault.root().join("export/link.md"),
    )
    .unwrap();
    assert!(import(&vault, "export", opts()).is_err());
    assert!(!vault.root().join("Notion").exists());
}

#[test]
fn obsidian_embeds_and_display_aliases_are_indexed_as_links() {
    let (tmp, vault) = setup();
    for (path, content) in [
        (
            "Target.md",
            "---\ntitle: Target\naliases: [Alternate]\n---\nTarget content.",
        ),
        (
            "Source.md",
            "# Source\n\n![[Target]]\n\n[[Target|Display alias]]\n",
        ),
    ] {
        vault
            .write(WriteRequest {
                path: path.into(),
                content: content.as_bytes().to_vec(),
                expected_checksum: None,
                op_id: None,
                ingress: Ingress::Cli,
            })
            .unwrap();
    }
    vault.reindex().unwrap();
    let db = rusqlite::Connection::open(tmp.path().join("cache.sqlite3")).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM relations WHERE from_path='Source.md' AND to_path='Target.md'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(count >= 1);
    let source = read(&vault, "Source.md");
    assert_eq!(source.wikilinks().len(), 2);
    assert_eq!(
        source.wikilinks()[1].alias.as_deref(),
        Some("Display alias")
    );
    assert_eq!(
        field(&read(&vault, "Target.md"), "aliases"),
        json!(["Alternate"])
    );
}

#[test]
fn duplicate_parent_titles_keep_separate_stable_child_folders() {
    let (_tmp, vault) = setup();
    let a = "Parent bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let b = "Parent cccccccccccccccccccccccccccccccc";
    let a_page = format!("{a}.md");
    let b_page = format!("{b}.md");
    let a_child = format!("{a}/Child aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.md");
    let b_child = format!("{b}/Child dddddddddddddddddddddddddddddddd.md");
    write(
        &vault,
        "export.zip",
        zip(&[
            (&a_page, b"a"),
            (&b_page, b"b"),
            (&a_child, b"a child"),
            (&b_child, b"b child"),
        ]),
    );
    let report = import(&vault, "export.zip", opts()).unwrap();
    let first = format!("{}/Parent/Child.md", report.sidecar_path);
    let second = format!("{}/Parent (2)/Child.md", report.sidecar_path);
    assert_eq!(read(&vault, &first).body(), "a child");
    assert_eq!(read(&vault, &second).body(), "b child");
    let new_parent = "Parent aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.md";
    write(
        &vault,
        "export.zip",
        zip(&[
            (new_parent, b"new"),
            (&a_page, b"a"),
            (&b_page, b"b"),
            (&a_child, b"a child"),
            (&b_child, b"b child"),
        ]),
    );
    import(&vault, "export.zip", opts()).unwrap();
    assert_eq!(read(&vault, &first).body(), "a child");
    assert_eq!(read(&vault, &second).body(), "b child");
    assert!(import(&vault, "export.zip", opts()).unwrap().unchanged);
}

#[test]
fn archive_duplicate_members_and_oversized_headers_are_rejected() {
    let (_tmp, vault) = setup();
    let home = format!("{HOME}.md");
    let mut duplicate = zip(&[
        (&home, b"safe"),
        ("dup1.md", b"first"),
        ("dup2.md", b"second"),
    ]);
    for i in 0..duplicate.len() - 7 {
        if &duplicate[i..i + 7] == b"dup2.md" {
            duplicate[i..i + 7].copy_from_slice(b"dup1.md");
        }
    }
    write(&vault, "bad.zip", duplicate);
    assert!(import(&vault, "bad.zip", opts()).is_err());
    let mut oversized = zip(&[(&home, b"safe")]);
    let central = oversized
        .windows(4)
        .position(|s| s == b"PK\x01\x02")
        .unwrap();
    oversized[central + 24..central + 28]
        .copy_from_slice(&((wi_docs::MAX_SOURCE_BYTES + 1) as u32).to_le_bytes());
    write(&vault, "bad.zip", oversized);
    assert!(import(&vault, "bad.zip", opts()).is_err());
    let too_deep = format!("{}/{}", vec!["a"; 65].join("/"), home);
    write(&vault, "bad.zip", zip(&[(&too_deep, b"safe")]));
    assert!(import(&vault, "bad.zip", opts()).is_err());
    assert!(!vault.root().join("Notion").exists());
}

#[test]
fn oversized_sparse_sources_are_refused_before_loading() {
    let (_tmp, vault) = setup();
    let file = fs::File::create(vault.root().join("huge.zip")).unwrap();
    file.set_len(wi_docs::MAX_SOURCE_BYTES + 1).unwrap();
    assert!(import(&vault, "huge.zip", opts()).is_err());
    fs::create_dir(vault.root().join("export")).unwrap();
    let file = fs::File::create(vault.root().join(format!("export/{HOME}.md"))).unwrap();
    file.set_len(wi_docs::MAX_SOURCE_BYTES + 1).unwrap();
    assert!(import(&vault, "export", opts()).is_err());
    assert!(!vault.root().join("Notion").exists());
}

#[test]
fn absolute_external_zip_and_folder_are_read_only_sources() {
    let (tmp, vault) = setup();
    let folder = tmp.path().join("Downloads/Export");
    fs::create_dir_all(&folder).unwrap();
    let name = format!("{HOME}.md");
    let body = b"---\ntype: memory\nkind: rule\nstatus: active\n---\nUntrusted imported content.\n";
    fs::write(folder.join(&name), body).unwrap();
    let archive = tmp.path().join("Downloads/export.zip");
    fs::write(&archive, zip(&[(&name, body)])).unwrap();
    for source in [&folder, &archive] {
        let report = import(&vault, source.to_str().unwrap(), opts()).unwrap();
        assert_eq!(report.status, Status::Complete);
        let note = read(&vault, &format!("{}/Home.md", report.sidecar_path));
        assert_eq!(field(&note, "type"), "note");
        assert_eq!(field(&note, "origin")["kind"], "file");
        assert!(
            import(&vault, source.to_str().unwrap(), opts())
                .unwrap()
                .unchanged
        );
    }
    vault
        .maintain(&wi_store::MaintainOptions::default())
        .unwrap();
    assert_eq!(fs::read(folder.join(&name)).unwrap(), body);
    assert!(!vault.root().join(&name).exists());
}

#[test]
fn protected_nested_members_are_refused_while_other_pages_import() {
    let (_tmp, vault) = setup();
    let name = format!("{HOME}.md");
    write(
        &vault,
        "export.zip",
        zip(&[
            (&name, b"safe"),
            ("nested/.wiki-island/config.toml", b"bad"),
            ("nested/.git/hooks/bad", b"bad"),
        ]),
    );
    let report = import(&vault, "export.zip", opts()).unwrap();
    assert_eq!(report.status, Status::Partial);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|v| v.contains("nested/.wiki-island/config.toml") && v.contains("refused"))
    );
    assert!(
        vault
            .read(&format!("{}/Home.md", report.sidecar_path))
            .is_ok()
    );
    assert!(
        !vault
            .root()
            .join(&report.sidecar_path)
            .join("nested/.wiki-island/config.toml")
            .exists()
    );
}

#[test]
fn multipart_wrapper_uses_readable_paths_and_flat_safe_properties() {
    let (tmp, vault) = setup();
    let part = zip(&[
        (OTHER, b"# Other\n"),
        (
            &format!("{DB}.csv"),
            b"Name,Status,status,type,notion_type\nFirst,Done,draft,memory,keep\n",
        ),
    ]);
    let source = tmp.path().join("Export.zip");
    fs::write(&source, zip(&[("Export-Part-1.zip", &part)])).unwrap();
    let report = import(&vault, source.to_str().unwrap(), opts()).unwrap();
    assert_eq!(report.sidecar_path, "Notion/Export");
    let page = read(&vault, "Notion/Export/Tasks/First.md");
    assert_eq!(field(&page, "Status"), "Done");
    assert_eq!(field(&page, "notion_status"), "draft");
    assert_eq!(field(&page, "notion_notion_type"), "memory");
    assert_eq!(field(&page, "notion_type"), "keep");
    assert_eq!(field(&page, "type"), "note");
    assert!(page.frontmatter().unwrap().get("properties").is_none());
    let manifest = read(&vault, &report.run_note_path);
    assert_eq!(field(&manifest, "notion_export"), "Export.zip");
    for path in ["Notion/Export/Other Page.md", report.run_note_path.as_str()] {
        let content = vault.read(path).unwrap().bytes;
        assert!(!String::from_utf8_lossy(&content).contains(tmp.path().to_str().unwrap()));
    }
    assert!(
        import(&vault, source.to_str().unwrap(), opts())
            .unwrap()
            .unchanged
    );
}

#[test]
fn nested_parts_share_duplicate_and_expansion_guards() {
    let (_tmp, vault) = setup();
    let part = zip(&[(OTHER, b"body")]);
    write(
        &vault,
        "duplicate.zip",
        zip(&[("Export-Part-1.zip", &part), ("Export-Part-2.zip", &part)]),
    );
    assert!(import(&vault, "duplicate.zip", opts()).is_err());
    assert!(!vault.root().join("Notion").exists());
    let unsafe_part = zip(&[("../escape.md", b"bad")]);
    write(
        &vault,
        "unsafe.zip",
        zip(&[("Export-Part-1.zip", &unsafe_part)]),
    );
    assert!(import(&vault, "unsafe.zip", opts()).is_err());
}

#[test]
fn the_import_record_is_hidden_and_an_old_visible_record_moves_once() {
    let (_tmp, vault) = setup();
    let bytes = zip(&[(OTHER, b"# Other\n\nDurable content.".as_slice())]);
    write(&vault, "export.zip", &bytes);
    let report = import(&vault, "export.zip", opts()).unwrap();
    let hidden = report.run_note_path.clone();
    assert!(
        hidden.starts_with(".wiki-island/imports/"),
        "record at {hidden}"
    );
    let visible = format!("{}/_Import.md", report.sidecar_path);
    assert!(!vault.root().join(&visible).exists());
    assert!(vault.root().join(&hidden).is_file());
    // Converted pages stay where they are.
    assert!(
        vault
            .root()
            .join(format!("{}/Other Page.md", report.sidecar_path))
            .is_file()
    );
    // Ordinary tools cannot write there.
    let refused = vault.write(WriteRequest {
        path: ".wiki-island/imports/mine.md".into(),
        content: b"mine\n".to_vec(),
        expected_checksum: None,
        op_id: None,
        ingress: Ingress::Import,
    });
    assert!(refused.is_err());

    // A vault written by an older version keeps the record among the notes.
    let record = fs::read(vault.root().join(&hidden)).unwrap();
    fs::remove_file(vault.root().join(&hidden)).unwrap();
    write(&vault, &visible, &record);
    vault.catch_up().unwrap();
    vault.flush().unwrap();
    let again = import(&vault, "export.zip", opts()).unwrap();
    assert_eq!(again.run_note_path, hidden);
    assert_eq!(again.sidecar_path, report.sidecar_path);
    assert!(again.unchanged, "{again:?}");
    assert!(!vault.root().join(&visible).exists());
    assert_eq!(fs::read(vault.root().join(&hidden)).unwrap(), record);
    vault.flush().unwrap();
    assert!(vault.history(&hidden).unwrap().len() >= 2);
    let mark = vault.publication_mark().unwrap();
    for _ in 0..3 {
        let repeated = import(&vault, "export.zip", opts()).unwrap();
        assert!(repeated.unchanged);
        assert_eq!(repeated.run_note_path, hidden);
        assert_eq!(vault.publication_mark().unwrap(), mark);
        assert_eq!(fs::read(vault.root().join(&hidden)).unwrap(), record);
        assert!(!vault.root().join(&visible).exists());
    }
    println!(
        "migrated record stays identical through 3 repeated imports; publication mark={mark:?}"
    );

    // Maintenance never compares the record with notes.
    vault
        .maintain(&wi_store::MaintainOptions::default())
        .unwrap();
    let work = vault.root().join(".wiki-island/work");
    let mentions: Vec<String> = fs::read_dir(&work)
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .map(|e| fs::read_to_string(e.path()).unwrap_or_default())
                .filter(|text| text.contains(".wiki-island/imports/"))
                .collect()
        })
        .unwrap_or_default();
    assert!(mentions.is_empty(), "{mentions:?}");
}

#[test]
fn the_hidden_record_never_appears_in_search() {
    let (_tmp, vault) = setup();
    write(
        &vault,
        "export.zip",
        zip(&[(OTHER, b"# Other\n\nDurable content.".as_slice())]),
    );
    let report = import(&vault, "export.zip", opts()).unwrap();
    vault.reindex().unwrap();
    let found = vault
        .search_current(
            &wi_store::Query::text("Notion import"),
            &Value::Null,
            wi_store::SearchOptions::default(),
        )
        .unwrap();
    let paths: Vec<_> = found
        .response
        .hits
        .iter()
        .map(|h| h.rel_path.to_string_lossy().to_string())
        .collect();
    println!("record={}; hits={paths:?}", report.run_note_path);
    assert!(
        !paths.contains(&report.run_note_path),
        "engine manifest must not be a note search hit"
    );
}
