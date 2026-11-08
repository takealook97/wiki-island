use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

fn command(tmp: &TempDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wi"));
    cmd.env("HOME", tmp.path().join("home"))
        .env("USERPROFILE", tmp.path().join("home"))
        .env("APPDATA", tmp.path().join("roaming"))
        .env("LOCALAPPDATA", tmp.path().join("local"))
        .env("WI_CACHE_DIR", tmp.path().join("cache"))
        .arg("--vault")
        .arg(tmp.path().join("vault"))
        .arg("--json");
    cmd
}
fn setup() -> TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let out = command(&tmp).arg("init").output().unwrap();
    assert!(out.status.success(), "{:?}", out);
    tmp
}
fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap()
}
fn png(path: &Path) {
    fs::write(
        path,
        [
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 0, 2,
            0, 0, 5, 0, 1, 165, 246, 69, 64, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ],
    )
    .unwrap();
}
#[test]
fn cli_without_tesseract_or_model_needs_ocr_without_failure() {
    let tmp = setup();
    png(&tmp.path().join("vault/scan.png"));
    for mode in ["off", "auto", "required"] {
        let out = command(&tmp)
            .env("PATH", tmp.path().join("empty-bin"))
            .args(["import", "scan.png", "--ocr", mode])
            .output()
            .unwrap();
        assert!(out.status.success(), "{:?}", out);
        let report = json(&out);
        assert_eq!(report["imports"][0]["status"], "needs_ocr");
    }
}
#[test]
fn cli_bulk_commit_noop_and_failure_exit() {
    let tmp = setup();
    for p in ["a.csv", "b.csv"] {
        fs::write(tmp.path().join("vault").join(p), "header,value\n").unwrap();
    }
    let out = command(&tmp)
        .args(["import", "a.csv", "b.csv", "--ocr", "off"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    assert_eq!(json(&out)["commits"], 1);
    let out = command(&tmp)
        .args(["import", "a.csv", "b.csv", "--ocr", "off"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(json(&out)["commits"], 0);
    assert_eq!(json(&out)["imports"][0]["unchanged"], true);
    let out = command(&tmp)
        .args(["import", "missing.csv", "a.csv", "--ocr", "off"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(json(&out)["imports"][0]["status"], "failed");
    assert_eq!(json(&out)["imports"][1]["unchanged"], true);
}
#[test]
fn stopped_bulk_writer_recovers_and_finishes_run_only_publication() {
    for fault in [
        "before_intent_fsync",
        "after_intent_before_swap",
        "after_swap_before_done",
        "after_done",
    ] {
        let tmp = setup();
        fs::write(tmp.path().join("vault/a.csv"), "header,value\n").unwrap();
        let out = command(&tmp)
            .env("WI_FAULT", fault)
            .args(["import", "a.csv", "--ocr", "off"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{fault}");
        let out = command(&tmp)
            .args(["import", "a.csv", "--ocr", "off"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{fault}: {:?}", out);
        let result = json(&out);
        assert!(result["blocked"].is_null(), "{fault}: {result}");
        assert_eq!(result["remaining"], 0);
        assert_eq!(result["imports"][0]["status"], "complete");
        assert!(tmp.path().join("vault/a.csv.md").is_file());
        assert_eq!(
            fs::read_to_string(tmp.path().join("vault/a.csv")).unwrap(),
            "header,value\n"
        );
        let doctor = command(&tmp).arg("doctor").output().unwrap();
        assert!(doctor.status.success(), "{:?}", doctor);
    }
}
#[cfg(unix)]
#[test]
fn local_tesseract_tsv_is_pinned_and_uses_a_snapshot() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = setup();
    png(&tmp.path().join("vault/scan.png"));
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let program = bin.join("tesseract");
    fs::write(&program,r#"#!/bin/sh
if [ "$1" = "--version" ]; then printf 'tesseract fixture-1\n'; exit 0; fi
case "$1" in */vault/*) exit 91;; esac
[ -f "$1" ] || exit 92
printf 'level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n5\t1\t1\t1\t1\t1\t0\t0\t1\t1\t95.0\tSnapshot\n'
"#).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let out = command(&tmp)
        .env("PATH", &bin)
        .args(["import", "scan.png", "--ocr", "required"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    let result = json(&out);
    assert_eq!(result["imports"][0]["status"], "complete");
    let sidecar = fs::read(tmp.path().join("vault/scan.png.md")).unwrap();
    let note = wi_note::Note::parse(&sidecar).unwrap();
    assert_eq!(note.body(), "Snapshot\n");
    let run_path = result["imports"][0]["run_note_path"].as_str().unwrap();
    let payload =
        wi_docs::read_run(&fs::read(tmp.path().join("vault").join(run_path)).unwrap()).unwrap();
    assert_eq!(payload.words.len(), 1);
    assert_eq!(payload.words[0].bbox, [0.0, 0.0, 1.0, 1.0]);
}

#[test]
fn real_local_tesseract_when_configured() {
    if std::env::var("WI_REAL_OCR").as_deref() != Ok("1") {
        return;
    }
    let tmp = setup();
    let (width, height, scale) = (720usize, 180usize, 12usize);
    let stride = (width * 3 + 3) & !3;
    let size = 54 + stride * height;
    let mut bmp = vec![255u8; size];
    bmp[..54].fill(0);
    bmp[..2].copy_from_slice(b"BM");
    bmp[2..6].copy_from_slice(&(size as u32).to_le_bytes());
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&(width as u32).to_le_bytes());
    bmp[22..26].copy_from_slice(&(height as u32).to_le_bytes());
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
    let letters = [
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
    for (letter, glyph) in letters.iter().enumerate() {
        for (row, pattern) in glyph.iter().enumerate() {
            for (column, bit) in pattern.bytes().enumerate() {
                if bit != b'1' {
                    continue;
                }
                for y in 0..scale {
                    for x in 0..scale {
                        let x = 60 + letter * 7 * scale + column * scale + x;
                        let y = 40 + row * scale + y;
                        let offset = 54 + (height - 1 - y) * stride + x * 3;
                        bmp[offset..offset + 3].fill(0);
                    }
                }
            }
        }
    }
    fs::write(tmp.path().join("vault/hello.bmp"), &bmp).unwrap();
    let out = command(&tmp)
        .args(["import", "hello.bmp", "--ocr", "auto"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    assert_eq!(json(&out)["imports"][0]["status"], "complete", "{:?}", out);
    let note =
        wi_note::Note::parse(&fs::read(tmp.path().join("vault/hello.bmp.md")).unwrap()).unwrap();
    assert!(
        note.body().to_ascii_uppercase().contains("HELLO"),
        "{}",
        note.body()
    );
    assert_eq!(fs::read(tmp.path().join("vault/hello.bmp")).unwrap(), bmp);
}
#[cfg(unix)]
#[test]
fn import_directory_prunes_unreadable_ignored_directory() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = setup();
    let vault = tmp.path().join("vault");
    let gitignore = vault.join(".gitignore");
    let mut rules = fs::read_to_string(&gitignore).unwrap_or_default();
    rules.push_str("\nignored/\n");
    fs::write(&gitignore, rules).unwrap();
    fs::write(vault.join("kept.csv"), "header,value\n").unwrap();
    let ignored = vault.join("ignored");
    fs::create_dir(&ignored).unwrap();
    fs::write(ignored.join("secret.csv"), "header,value\n").unwrap();
    fs::set_permissions(&ignored, fs::Permissions::from_mode(0o000)).unwrap();
    let out = command(&tmp)
        .args(["import", ".", "--ocr", "off"])
        .output()
        .unwrap();
    fs::set_permissions(&ignored, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(out.status.success(), "{out:?}");
    let report = json(&out);
    let sources: Vec<&str> = report["imports"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["source"].as_str().or(i["path"].as_str()))
        .collect();
    assert_eq!(report["imports"].as_array().unwrap().len(), 1, "{report}");
    assert!(sources.iter().all(|s| !s.contains("ignored")), "{report}");
}

/// A stored (uncompressed) ZIP archive, enough for a synthetic Notion export.
fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, bytes) in entries {
        let offset = out.len() as u32;
        let (crc, size, len) = (crc32(bytes), bytes.len() as u32, name.len() as u16);
        out.extend_from_slice(b"PK\x03\x04\x14\x00\x00\x08\x00\x00\x00\x00\x00\x00");
        for v in [crc, size, size] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(bytes);
        central.extend_from_slice(b"PK\x01\x02\x14\x00\x14\x00\x00\x08\x00\x00\x00\x00\x00\x00");
        for v in [crc, size, size] {
            central.extend_from_slice(&v.to_le_bytes());
        }
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let (start, size, count) = (out.len() as u32, central.len() as u32, entries.len() as u16);
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00");
    for v in [count, count] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in [size, start] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[test]
fn notion_export_folder_and_zip_import_through_the_cli() {
    let tmp = setup();
    let vault = tmp.path().join("vault");
    let home = "Home 11111111111111111111111111111111";
    let page = format!(
        "# Home\n\n![picture](photo.png)\n\n[Plan]({}/Plan%2022222222222222222222222222222222.md)\n",
        home.replace(' ', "%20")
    );
    let child = "# Plan\n\nShip it.\n";
    let export = vault.join("Notion Export");
    fs::create_dir_all(export.join(home)).unwrap();
    fs::write(export.join(format!("{home}.md")), &page).unwrap();
    fs::write(
        export
            .join(home)
            .join("Plan 22222222222222222222222222222222.md"),
        child,
    )
    .unwrap();
    png(&export.join("photo.png"));

    // The folder is converted as a whole, not expanded into its files.
    let out = command(&tmp)
        .args(["import", "Notion Export", "--ocr", "off"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    let report = json(&out);
    let imports = report["imports"].as_array().unwrap();
    assert_eq!(imports.len(), 1, "{report}");
    assert_eq!(imports[0]["status"], "complete", "{report}");
    let prefix = imports[0]["sidecar_path"].as_str().unwrap().to_owned();
    let converted = fs::read_to_string(vault.join(&prefix).join("Home.md")).unwrap();
    assert!(
        converted.contains(&format!("![[{prefix}/photo.png|picture]]")),
        "{converted}"
    );
    assert!(
        converted.contains(&format!("[[{prefix}/Home/Plan.md|Plan]]")),
        "{converted}"
    );
    assert!(vault.join(&prefix).join("photo.png").is_file());

    // A ZIP export goes the same way, attachments included, and the copies are committed.
    let zipped = stored_zip(&[
        (&format!("{home}.md"), page.as_bytes()),
        (
            &format!("{home}/Plan 22222222222222222222222222222222.md"),
            child.as_bytes(),
        ),
        ("photo.png", b"zip picture"),
    ]);
    fs::write(vault.join("export.zip"), zipped).unwrap();
    let out = command(&tmp)
        .args(["import", "export.zip", "--ocr", "off"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    let report = json(&out);
    assert_eq!(report["imports"][0]["status"], "complete", "{report}");
    let prefix = report["imports"][0]["sidecar_path"].as_str().unwrap();
    assert_eq!(
        fs::read(vault.join(prefix).join("photo.png")).unwrap(),
        b"zip picture"
    );
    let out = command(&tmp)
        .args(["history", &format!("{prefix}/photo.png")])
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    assert_eq!(
        json(&out).as_array().map(Vec::len),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}
