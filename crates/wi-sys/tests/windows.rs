//! Windows behavior of the platform layer. Each test proves one property
//! and removes what it creates.
#![cfg(windows)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use wi_sys::fs::{Dir, Kind, is_reparse_refusal, is_sharing_violation};
use wi_sys::process::{Liveness, liveness, spawn_in_job};

/// Environment variable that turns `helper_process` into a helper role.
const HELPER: &str = "WI_SYS_TEST_HELPER";

fn mklink_junction(link: &Path, target: &Path) {
    let status = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J failed");
}

fn read_all(dir: &Dir, name: &str) -> String {
    let mut s = String::new();
    dir.open_file(name, false)
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    s
}

fn write_new(dir: &Dir, name: &str, text: &str) {
    let mut f = dir.create_file(name).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// Assert that every `Dir` method refuses or does not follow `name`, a
/// reparse point inside `dir` that points at `outside`.
fn assert_reparse_refused(dir: &Dir, name: &str, outside: &Path) {
    let refused = |r: std::io::Result<()>, what: &str| {
        let e = r.expect_err(what);
        assert!(is_reparse_refusal(&e), "{what}: {e:?}");
    };
    refused(dir.open_dir(name).map(drop), "open_dir");
    refused(dir.open_file(name, false).map(drop), "open_file read");
    refused(dir.open_file(name, true).map(drop), "open_file write");
    refused(dir.unlink(name), "unlink");
    refused(dir.remove_dir(name), "remove_dir");
    refused(dir.rename(name, dir, "moved", false), "rename");
    let e = dir.create_dir(name).expect_err("create_dir");
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists, "{e:?}");
    let e = dir.create_file(name).expect_err("create_file");
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists, "{e:?}");
    let stat = dir.stat(name).unwrap().unwrap();
    assert_eq!(stat.kind, Kind::Reparse);
    let listed = dir.list().unwrap();
    assert!(
        listed.contains(&(name.to_owned(), Kind::Reparse)),
        "{listed:?}"
    );
    assert!(dir.stat("moved").unwrap().is_none());
    // The link and its target are untouched.
    assert!(std::fs::symlink_metadata(dir.final_path().unwrap().join(name)).is_ok());
    assert!(std::fs::symlink_metadata(outside).is_ok());
}

#[test]
fn names_must_be_single_components() {
    let t = tempfile::tempdir().unwrap();
    let dir = Dir::open_root(t.path()).unwrap();
    for bad in ["", ".", "..", "a\\b", "a/b", "a:b", "a\0b"] {
        let e = dir.open_dir(bad).expect_err(bad);
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}");
        let e = dir.create_file(bad).expect_err(bad);
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}");
    }
}

#[test]
fn a_junction_is_refused_by_every_method() {
    let t = tempfile::tempdir().unwrap();
    let outside = t.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "outside").unwrap();
    let vault = t.path().join("vault");
    std::fs::create_dir(&vault).unwrap();
    mklink_junction(&vault.join("link"), &outside);
    let dir = Dir::open_root(&vault).unwrap();
    assert_reparse_refused(&dir, "link", &outside);
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
}

#[test]
fn a_root_that_is_a_junction_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let outside = t.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let link = t.path().join("link");
    mklink_junction(&link, &outside);
    let e = Dir::open_root(&link).expect_err("junction root");
    assert!(is_reparse_refusal(&e), "{e:?}");
}

#[test]
fn file_and_directory_symlinks_are_refused_by_every_method() {
    let t = tempfile::tempdir().unwrap();
    let outside = t.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "outside").unwrap();
    let vault = t.path().join("vault");
    std::fs::create_dir(&vault).unwrap();
    std::os::windows::fs::symlink_dir(&outside, vault.join("dirlink")).unwrap();
    std::os::windows::fs::symlink_file(outside.join("secret.txt"), vault.join("filelink")).unwrap();
    let dir = Dir::open_root(&vault).unwrap();
    assert_reparse_refused(&dir, "dirlink", &outside);
    assert_reparse_refused(&dir, "filelink", &outside.join("secret.txt"));
    assert_eq!(
        std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
        "outside"
    );
}

#[test]
fn create_without_replace_never_overwrites() {
    let t = tempfile::tempdir().unwrap();
    let dir = Dir::open_root(t.path()).unwrap();
    write_new(&dir, "a.md", "first");
    let e = dir.create_file("a.md").expect_err("exists");
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(read_all(&dir, "a.md"), "first");
    dir.create_dir("sub").unwrap();
    let e = dir.create_dir("sub").expect_err("exists");
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
}

#[test]
fn rename_replaces_only_when_asked() {
    let t = tempfile::tempdir().unwrap();
    let dir = Dir::open_root(t.path()).unwrap();
    let sub = dir.create_dir("sub").unwrap();
    write_new(&dir, "new.tmp", "new");
    write_new(&sub, "target.md", "old");
    let e = dir
        .rename("new.tmp", &sub, "target.md", false)
        .expect_err("no replace");
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists, "{e:?}");
    assert_eq!(read_all(&sub, "target.md"), "old");
    assert_eq!(read_all(&dir, "new.tmp"), "new");
    dir.rename("new.tmp", &sub, "target.md", true).unwrap();
    assert_eq!(read_all(&sub, "target.md"), "new");
    assert!(dir.stat("new.tmp").unwrap().is_none());
    // Without replace into a free name it creates the target.
    sub.rename("target.md", &dir, "back.md", false).unwrap();
    assert_eq!(read_all(&dir, "back.md"), "new");
}

#[test]
fn rename_replaces_a_target_that_is_open_with_shared_delete() {
    let t = tempfile::tempdir().unwrap();
    let dir = Dir::open_root(t.path()).unwrap();
    write_new(&dir, "new.tmp", "new");
    write_new(&dir, "target.md", "old");
    let held = dir.open_file("target.md", false).unwrap();
    dir.rename("new.tmp", &dir, "target.md", true).unwrap();
    drop(held);
    assert_eq!(read_all(&dir, "target.md"), "new");
}

#[test]
fn unlink_and_remove_dir_delete_relative_to_the_handle() {
    let t = tempfile::tempdir().unwrap();
    let dir = Dir::open_root(t.path()).unwrap();
    write_new(&dir, "a.md", "x");
    let held = dir.open_file("a.md", false).unwrap();
    dir.unlink("a.md").unwrap();
    // POSIX semantics: the name is gone while a handle is still open.
    assert!(dir.stat("a.md").unwrap().is_none());
    drop(held);
    let sub = dir.create_dir("sub").unwrap();
    drop(sub);
    let e = dir.unlink("sub").expect_err("unlink a directory");
    assert_eq!(e.kind(), std::io::ErrorKind::IsADirectory, "{e:?}");
    dir.remove_dir("sub").unwrap();
    assert!(dir.stat("sub").unwrap().is_none());
    // A read-only file is deleted like on Unix.
    write_new(&dir, "ro.md", "x");
    let path = t.path().join("ro.md");
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).unwrap();
    dir.unlink("ro.md").unwrap();
    assert!(dir.stat("ro.md").unwrap().is_none());
}

#[test]
fn unlink_of_a_file_open_without_shared_delete_is_a_sharing_violation() {
    let t = tempfile::tempdir().unwrap();
    let dir = Dir::open_root(t.path()).unwrap();
    write_new(&dir, "busy.md", "x");
    // FILE_SHARE_READ | FILE_SHARE_WRITE, no FILE_SHARE_DELETE.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2)
        .open(t.path().join("busy.md"))
        .unwrap();
    let e = dir.unlink("busy.md").expect_err("in use");
    assert!(is_sharing_violation(&e), "{e:?}");
    write_new(&dir, "next.tmp", "y");
    let e = dir
        .rename("next.tmp", &dir, "busy.md", true)
        .expect_err("replace in use");
    assert!(is_sharing_violation(&e), "{e:?}");
    drop(held);
    assert_eq!(read_all(&dir, "busy.md"), "x");
    dir.unlink("busy.md").unwrap();
}

#[test]
fn handle_relative_creation_reaches_beyond_max_path() {
    let t = tempfile::tempdir().unwrap();
    let root = Dir::open_root(t.path()).unwrap();
    let segment = "d".repeat(60);
    let mut chain = vec![root.create_dir(&segment).unwrap()];
    for _ in 0..5 {
        let next = chain.last().unwrap().create_dir(&segment).unwrap();
        chain.push(next);
    }
    let deepest = chain.last().unwrap();
    let final_path = deepest.final_path().unwrap();
    assert!(final_path.as_os_str().len() > 300, "{final_path:?}");
    write_new(deepest, "note.md", "deep");
    assert_eq!(read_all(deepest, "note.md"), "deep");
    deepest.unlink("note.md").unwrap();
    for i in (1..chain.len()).rev() {
        chain[i - 1].remove_dir(&segment).unwrap();
    }
    root.remove_dir(&segment).unwrap();
    assert!(root.list().unwrap().is_empty());
}

#[test]
fn list_reports_names_and_kinds() {
    let t = tempfile::tempdir().unwrap();
    let dir = Dir::open_root(t.path()).unwrap();
    write_new(&dir, "a.md", "x");
    dir.create_dir("sub").unwrap();
    let mut listed = dir.list().unwrap();
    listed.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        listed,
        vec![
            ("a.md".to_owned(), Kind::File),
            ("sub".to_owned(), Kind::Dir)
        ]
    );
    // Many entries span more than one listing buffer.
    let sub = dir.open_dir("sub").unwrap();
    for i in 0..600 {
        write_new(&sub, &format!("{i:04}-{}.md", "n".repeat(40)), "");
    }
    assert_eq!(sub.list().unwrap().len(), 600);
}

#[test]
fn long_name_returns_the_on_disk_name() {
    let t = tempfile::tempdir().unwrap();
    let dir = Dir::open_root(t.path()).unwrap();
    write_new(&dir, "Mixed Case Name.md", "x");
    assert_eq!(
        dir.long_name("mixed case name.MD").unwrap(),
        "Mixed Case Name.md"
    );
    // A short (8.3) alias, when the volume generates one.
    let output = Command::new("cmd")
        .args(["/C", "dir", "/x"])
        .current_dir(t.path())
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&output.stdout);
    if let Some(short) = listing
        .split_whitespace()
        .find(|w| w.contains('~') && w.to_ascii_uppercase().ends_with(".MD"))
    {
        assert_eq!(dir.long_name(short).unwrap(), "Mixed Case Name.md");
    }
}

#[test]
fn stamp_stat_and_flush_agree() {
    let t = tempfile::tempdir().unwrap();
    let dir = Dir::open_root(t.path()).unwrap();
    let mut f = dir.create_file("a.md").unwrap();
    f.write_all(b"hello").unwrap();
    wi_sys::fs::flush_file(&f).unwrap();
    let stamped = wi_sys::fs::stamp(&f).unwrap();
    drop(f);
    let stat = dir.stat("a.md").unwrap().unwrap();
    assert_eq!(stamped, stat);
    assert_eq!(stat.kind, Kind::File);
    assert_eq!(stat.size, 5);
    assert_ne!(stat.file_id, 0);
    assert!(!stat.readonly);
    write_new(&dir, "b.md", "");
    assert_ne!(dir.stat("b.md").unwrap().unwrap().file_id, stat.file_id);
    assert_eq!(dir.stat("sub").unwrap(), None);
    // A write-capable reopen flushes; a read-only file is skipped.
    assert!(dir.open_file("a.md", true).is_ok());
    let path = t.path().join("a.md");
    assert!(wi_sys::fs::flush_path(&path).unwrap());
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).unwrap();
    assert!(!wi_sys::fs::flush_path(&path).unwrap());
    assert!(dir.stat("a.md").unwrap().unwrap().readonly);
    assert!(dir.flush().is_ok());
    dir.unlink("a.md").unwrap();
}

#[test]
fn liveness_tells_running_from_exited() {
    assert_eq!(liveness(std::process::id()), Liveness::Alive);
    assert_eq!(liveness(0), Liveness::Unknown);
    let mut child = Command::new("cmd").args(["/C", "exit 0"]).spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    assert_eq!(liveness(pid), Liveness::Gone);
}

/// Helper roles, selected by `HELPER`; a plain test run does nothing.
#[test]
fn helper_process() {
    match std::env::var(HELPER).as_deref() {
        Ok("parent") => {
            // Start a grandchild at once, report its pid, then wait.
            let mut grandchild = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "helper_process", "--nocapture"])
                .env(HELPER, "sleep")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            println!("GRANDCHILD={}", grandchild.id());
            std::io::stdout().flush().unwrap();
            std::thread::sleep(Duration::from_secs(60));
            let _ = grandchild.kill();
            let _ = grandchild.wait();
        }
        Ok("sleep") => std::thread::sleep(Duration::from_secs(60)),
        Ok("echo") => {
            let code = if wi_sys::console::EchoOff::new().is_err() {
                0
            } else {
                3
            };
            std::process::exit(code);
        }
        _ => {}
    }
}

fn helper(role: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "helper_process",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(HELPER, role);
    cmd
}

/// Spawn the parent helper in a job and return it with its grandchild pid.
fn spawn_tree() -> (std::process::Child, wi_sys::process::Job, u32) {
    let mut cmd = helper("parent");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let (mut child, job) = spawn_in_job(&mut cmd).unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut pid = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.unwrap();
        if let Some(rest) = line.strip_prefix("GRANDCHILD=") {
            pid = Some(rest.trim().parse::<u32>().unwrap());
            break;
        }
    }
    let pid = pid.expect("helper reported no grandchild");
    assert_eq!(liveness(pid), Liveness::Alive);
    (child, job, pid)
}

fn wait_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while liveness(pid) != Liveness::Gone {
        assert!(Instant::now() < deadline, "process {pid} survived the job");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn terminating_the_job_kills_a_grandchild() {
    let (mut child, job, grandchild) = spawn_tree();
    job.terminate(1).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(1));
    wait_gone(grandchild);
}

#[test]
fn dropping_the_job_kills_a_grandchild() {
    let (mut child, job, grandchild) = spawn_tree();
    drop(job);
    child.wait().unwrap();
    wait_gone(grandchild);
}

#[test]
fn spawn_in_job_resets_creation_flags_for_reuse() {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "exit 7"]);
    let (mut child, _job) = spawn_in_job(&mut cmd).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(7));
    // A plain spawn of the same Command is not left suspended.
    assert_eq!(cmd.status().unwrap().code(), Some(7));
}

#[test]
fn credential_round_trip() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let target = format!("wi-sys-test/{}-{nanos}", std::process::id());
    use wi_sys::credential::{delete, read, write};
    assert_eq!(read(&target).unwrap(), None);
    write(&target, "sécret token").unwrap();
    let got = read(&target);
    let replaced = write(&target, "second");
    let got2 = read(&target);
    let deleted = delete(&target);
    assert_eq!(got.unwrap().as_deref(), Some("sécret token"));
    replaced.unwrap();
    assert_eq!(got2.unwrap().as_deref(), Some("second"));
    assert!(deleted.unwrap());
    assert_eq!(read(&target).unwrap(), None);
    assert!(!delete(&target).unwrap());
    assert!(write(&target, &"x".repeat(5 * 512 + 1)).is_err());
    assert_eq!(read(&target).unwrap(), None);
}

#[test]
fn echo_off_refuses_a_non_console_stdin() {
    let status = helper("echo")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    let status = helper("echo")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
}
