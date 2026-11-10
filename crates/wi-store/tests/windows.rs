//! Windows counterparts of the Unix containment and placement guarantees
//! (D5-D10, D22): reparse points inside the vault are refused for read,
//! write, list and move; Windows-reserved names are refused; `\` input is
//! accepted and stored with `/`; placement never overwrites a file that
//! appeared or changed after the check.
#![cfg(windows)]

mod common;

use std::path::Path;
use std::process::Command;
use std::rc::Rc;

use common::{Fixture, write};
use wi_store::{Hooks, Ingress, MaintainOptions, MoveRequest, StoreError, WriteRequest};

fn junction(link: &Path, target: &Path) {
    let status = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J failed");
}

fn request(path: &str, content: &str, expected: Option<String>) -> WriteRequest {
    WriteRequest {
        path: path.into(),
        content: content.as_bytes().to_vec(),
        expected_checksum: expected,
        op_id: None,
        ingress: Ingress::Cli,
    }
}

fn move_request(from: &str, to: &str) -> MoveRequest {
    MoveRequest {
        from: from.into(),
        to: to.into(),
        expected_checksum: None,
        op_id: None,
        ingress: Ingress::Cli,
    }
}

#[test]
fn a_junction_inside_the_vault_is_refused_for_read_write_list_and_move() {
    let (fx, vault) = Fixture::new();
    let outside = fx.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("a.md"), "# secret\n").unwrap();
    junction(&vault.root().join("link"), &outside);
    write(&vault, "x.md", "# x\n", Ingress::Cli);

    assert!(vault.read("link/a.md").is_err(), "read through a junction");
    let absolute = vault.root().join("link").join("a.md");
    assert!(
        vault.read(absolute.to_str().unwrap()).is_err(),
        "absolute path through a junction"
    );
    assert!(vault.read("LINK\\a.md").is_err(), "case and backslash");
    assert!(
        vault.name_stat("link/a.md").is_err(),
        "stat through a junction"
    );
    assert!(
        vault.write(request("link/b.md", "# b\n", None)).is_err(),
        "write through a junction"
    );
    assert!(!outside.join("b.md").exists());
    assert!(
        vault.move_note(move_request("x.md", "link/x.md")).is_err(),
        "move into a junction"
    );
    assert!(
        vault
            .move_note(move_request("link/a.md", "moved.md"))
            .is_err(),
        "move out of a junction"
    );
    assert!(!outside.join("x.md").exists());
    assert!(vault.root().join("x.md").exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("a.md")).unwrap(),
        "# secret\n"
    );

    vault
        .maintain(&MaintainOptions {
            full: true,
            ..Default::default()
        })
        .unwrap();
    let rows = vault.name_rows().unwrap().unwrap_or_default();
    assert!(
        rows.iter().all(|row| !row.path.starts_with("link")),
        "{:?}",
        rows.iter().map(|r| &r.path).collect::<Vec<_>>()
    );
}

#[test]
fn a_file_symlink_inside_the_vault_is_refused() {
    let (fx, vault) = Fixture::new();
    let outside = fx.dir.path().join("secret.md");
    std::fs::write(&outside, "# secret\n").unwrap();
    std::os::windows::fs::symlink_file(&outside, vault.root().join("s.md")).unwrap();
    assert!(vault.read("s.md").is_err());
    assert!(vault.write(request("s.md", "# new\n", None)).is_err());
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "# secret\n");
}

#[test]
fn windows_reserved_names_are_refused_and_create_nothing() {
    let (_fx, vault) = Fixture::new();
    for path in [
        "CON.md",
        "con",
        "notes/nul.txt",
        "Aux.md",
        "PRN",
        "COM1.md",
        "lpt9.txt",
        "COM0.md",
        "COM\u{b9}.md",
        "LPT\u{b2}",
        "a:b.md",
        "a.md:stream",
        "a<b.md",
        "a>b.md",
        "a\"b.md",
        "a|b.md",
        "a?b.md",
        "a*b.md",
        "trailing.md.",
        "trailing.md ",
        "dir./a.md",
        "dir /a.md",
    ] {
        let result = vault.write(request(path, "# x\n", None));
        assert!(result.is_err(), "{path} was accepted: {result:?}");
    }
    let entries: Vec<String> = std::fs::read_dir(vault.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    assert!(entries.is_empty(), "{entries:?}");
    // Ordinary names that only look similar stay allowed.
    for path in ["console.md", "com10.md", "lpt.md", "nul-note.md", "a.b.md"] {
        write(&vault, path, "# ok\n", Ingress::Cli);
    }
}

#[test]
fn backslash_input_is_accepted_and_stored_with_slashes() {
    let (_fx, vault) = Fixture::new();
    let result = write(&vault, "notes\\daily\\a.md", "# a\n", Ingress::Cli);
    assert_eq!(result.path, "notes/daily/a.md");
    assert_eq!(
        vault.read("notes\\daily\\a.md").unwrap().path,
        "notes/daily/a.md"
    );
    let absolute = vault.root().join("notes\\daily\\a.md");
    assert_eq!(
        vault.read(absolute.to_str().unwrap()).unwrap().path,
        "notes/daily/a.md"
    );
    let upper = absolute.to_str().unwrap().to_uppercase();
    assert!(vault.read(&upper).is_ok(), "{upper}");
    let verbatim = format!("\\\\?\\{}", absolute.display());
    assert_eq!(vault.read(&verbatim).unwrap().path, "notes/daily/a.md");
    for refused in [
        "C:notes\\a.md",
        "\\notes\\a.md",
        "\\\\.\\C:\\x.md",
        "..\\x.md",
    ] {
        assert!(vault.read(refused).is_err(), "{refused}");
    }
    let root = vault.root().to_str().unwrap().to_owned();
    let lookalike = format!("{root}2\\a.md");
    assert!(
        matches!(vault.read(&lookalike), Err(StoreError::Protected { .. })),
        "{lookalike}"
    );
}

#[test]
fn a_create_never_overwrites_a_file_that_appeared_after_the_check() {
    let (_fx, mut vault) = Fixture::new();
    vault.set_hooks(Hooks {
        before_swap: Some(Rc::new(|path| {
            std::fs::write(path, "editor bytes\n").unwrap();
        })),
        ..Default::default()
    });
    let result = vault.write(request("new.md", "# engine\n", None));
    assert!(
        matches!(result, Err(StoreError::Stale { .. })),
        "{result:?}"
    );
    assert_eq!(
        std::fs::read_to_string(vault.root().join("new.md")).unwrap(),
        "editor bytes\n"
    );
}

#[test]
fn a_replacement_is_stale_when_the_target_changed_before_placement() {
    let (_fx, mut vault) = Fixture::new();
    let first = write(&vault, "note.md", "# one\n", Ingress::Cli);
    vault.set_hooks(Hooks {
        before_swap: Some(Rc::new(|path| {
            std::fs::write(path, "# editor\n").unwrap();
        })),
        ..Default::default()
    });
    let result = vault.write(request("note.md", "# two\n", Some(first.checksum)));
    assert!(
        matches!(result, Err(StoreError::Stale { .. })),
        "{result:?}"
    );
    assert_eq!(
        std::fs::read_to_string(vault.root().join("note.md")).unwrap(),
        "# editor\n"
    );
}

#[test]
fn a_replacement_is_stale_when_the_target_was_deleted_before_placement() {
    let (_fx, mut vault) = Fixture::new();
    let first = write(&vault, "gone.md", "# one\n", Ingress::Cli);
    vault.set_hooks(Hooks {
        before_swap: Some(Rc::new(|path| {
            std::fs::remove_file(path).unwrap();
        })),
        ..Default::default()
    });
    let result = vault.write(request("gone.md", "# two\n", Some(first.checksum)));
    assert!(
        matches!(result, Err(StoreError::Stale { .. })),
        "{result:?}"
    );
    assert!(!vault.root().join("gone.md").exists());
}

#[test]
fn a_note_round_trips_through_write_read_move_and_maintain() {
    let (fx, vault) = Fixture::new();
    let created = write(&vault, "notes/a.md", "# a\n\nbody\n", Ingress::Cli);
    let replaced = vault
        .write(request(
            "notes/a.md",
            "# a\n\nbody two\n",
            Some(created.checksum),
        ))
        .unwrap();
    let read = vault.read("notes/a.md").unwrap();
    assert_eq!(read.checksum, replaced.checksum);
    assert!(String::from_utf8(read.bytes).unwrap().contains("body two"));
    let moved = vault
        .move_note(move_request("notes/a.md", "archive/a.md"))
        .unwrap();
    assert_eq!(moved.path, "archive/a.md");
    assert!(!vault.root().join("notes/a.md").exists());
    std::fs::write(vault.root().join("outside-editor.md"), "# hand\n").unwrap();
    vault
        .maintain(&MaintainOptions {
            full: true,
            ..Default::default()
        })
        .unwrap();
    drop(vault);
    let vault = fx.open();
    assert!(vault.read("archive/a.md").is_ok());
    assert!(vault.read("outside-editor.md").is_ok());
    let rows = vault.name_rows().unwrap().unwrap_or_default();
    let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"archive/a.md"), "{paths:?}");
    assert!(paths.contains(&"outside-editor.md"), "{paths:?}");
    assert!(vault.doctor().is_ok());
}

/// D7b: a short (8.3) alias of an existing folder is refused before any
/// change, so protected-path checks always see the real long name. Runs
/// only where the volume generates short names.
#[test]
fn a_short_name_alias_is_refused() {
    let (_fx, vault) = Fixture::new();
    write(&vault, "Long Folder Name/a.md", "# a\n", Ingress::Cli);
    let long = vault.root().join("Long Folder Name");
    let out = Command::new("cmd")
        .arg("/C")
        .arg(format!("for %I in (\"{}\") do @echo %~sI", long.display()))
        .output()
        .unwrap();
    let short_path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let Some(short) = short_path.rsplit('\\').next().filter(|s| s.contains('~')) else {
        eprintln!("no short names on this volume: {short_path:?}");
        return;
    };
    let alias = format!("{short}/b.md");
    assert!(
        vault.write(request(&alias, "# b\n", None)).is_err(),
        "{alias}"
    );
    assert!(!long.join("b.md").exists());
    assert!(vault.read(&format!("{short}/a.md")).is_err());

    // A short name explicitly assigned on NTFS need not contain `~`.
    // Check the same containment rule for that spelling as well.
    let output = Command::new("fsutil")
        .args(["file", "setshortname"])
        .arg(&long)
        .arg("ALIAS")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(vault.write(request("ALIAS/b.md", "# b\n", None)).is_err());
    assert!(vault.read("ALIAS/a.md").is_err());
    assert!(!long.join("b.md").exists());
    assert_eq!(vault.read("Long Folder Name/a.md").unwrap().bytes, b"# a\n");
}

#[test]
fn a_bounded_read_refuses_a_file_above_the_limit_and_still_refuses_a_junction() {
    let (fx, vault) = Fixture::new();
    drop(vault);
    let config = fx.root_input().join(".wiki-island").join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("max_note_bytes = 16\n");
    std::fs::write(&config, text).unwrap();
    let vault = fx.open();
    std::fs::write(vault.root().join("over.bin"), [7u8; 17]).unwrap();
    std::fs::write(vault.root().join("at.md"), [b'a'; 16]).unwrap();

    match vault.read_bounded("over.bin").unwrap_err() {
        StoreError::TooLarge { path, size, limit } => {
            assert_eq!((path.as_str(), size, limit), ("over.bin", 17, 16));
        }
        other => panic!("{other}"),
    }
    assert_eq!(vault.read_bounded("at.md").unwrap().bytes.len(), 16);
    assert_eq!(vault.read("over.bin").unwrap().bytes.len(), 17);

    let outside = fx.dir.path().join("outside-bounded");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("a.md"), "# secret\n").unwrap();
    junction(&vault.root().join("link"), &outside);
    assert!(
        vault.read_bounded("link/a.md").is_err(),
        "bounded read through a junction"
    );
}
