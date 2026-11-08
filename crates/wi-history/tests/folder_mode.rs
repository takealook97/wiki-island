mod common;

use std::path::Path;
use std::process::Command;

use common::*;
use wi_history::{Flusher, HistoryError, StorageMode, Vault, local_state_dir, storage_mode};

fn folder_vault(id: &str) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("notes");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(root.join(".wiki-island")).unwrap();
    std::fs::write(
        root.join(".wiki-island/config.toml"),
        format!("vault_id = \"{id}\"\n"),
    )
    .unwrap();
    (tmp, root, data)
}

fn git_external(git_dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "core.autocrlf=false"])
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn git_worktree_path(text: &str) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        Path::new(text.trim()).canonicalize().unwrap()
    }
    #[cfg(not(windows))]
    {
        Path::new(text.trim()).to_path_buf()
    }
}

#[test]
fn folder_mode_commits_through_core_worktree_without_git_in_the_folder() {
    let (_tmp, root, data) = folder_vault("v-folder");
    let vault = Vault::init_folder(&root, Some(&data)).unwrap();
    assert_eq!(vault.storage_mode(), StorageMode::Folder);
    assert!(!root.join(".git").exists());
    let git_dir = data.join("vaults/v-folder/git");
    assert_eq!(vault.git_dir(), git_dir.canonicalize().unwrap());
    assert!(vault.wi_dir().starts_with(vault.git_dir()));

    save(&vault, "notes/a.md", b"a1\n", "cli", Some("op-1"));
    let report = Flusher::flush(&vault).unwrap();
    assert_eq!(report.commits.len(), 1);
    assert_eq!(git_external(&git_dir, &["show", "HEAD:notes/a.md"]), "a1\n");
    // The system git agrees the working tree is the notes folder.
    let top = git_external(&git_dir, &["rev-parse", "--show-toplevel"]);
    assert_eq!(git_worktree_path(&top), root.canonicalize().unwrap());
    let status = git_external(&git_dir, &["status", "--porcelain", "--", "notes"]);
    assert!(status.is_empty(), "{status}");

    drop(vault);
    let reopened = Vault::open_in(&root, Some(&data)).unwrap();
    assert_eq!(reopened.storage_mode(), StorageMode::Folder);
    assert_eq!(
        storage_mode(&root, Some(&data)).unwrap(),
        Some(StorageMode::Folder)
    );
    assert_eq!(
        local_state_dir(&root, Some(&data)).unwrap(),
        data.join("vaults/v-folder/local")
    );
    assert!(reopened.rebound_from().is_none());
}

#[test]
fn folder_mode_init_is_idempotent_and_ignores_a_stale_staging_dir() {
    let (_tmp, root, data) = folder_vault("v-stage");
    let staging = data.join("vaults/v-stage/git.tmp");
    std::fs::create_dir_all(staging.join("junk")).unwrap();
    let vault = Vault::init_folder(&root, Some(&data)).unwrap();
    assert!(!staging.exists());
    save(&vault, "a.md", b"a\n", "cli", None);
    Flusher::flush(&vault).unwrap();
    drop(vault);
    let again = Vault::init_folder(&root, Some(&data)).unwrap();
    assert_eq!(again.storage_mode(), StorageMode::Folder);
    assert_eq!(
        git_external(&data.join("vaults/v-stage/git"), &["show", "HEAD:a.md"]),
        "a\n"
    );
}

#[test]
fn moved_folder_vault_is_rebound_and_remembered_for_doctor() {
    let (tmp, root, data) = folder_vault("v-move");
    let vault = Vault::init_folder(&root, Some(&data)).unwrap();
    save(&vault, "a.md", b"a\n", "cli", None);
    Flusher::flush(&vault).unwrap();
    drop(vault);
    let old = root.canonicalize().unwrap();
    let moved = tmp.path().join("moved");
    std::fs::rename(&root, &moved).unwrap();
    let vault = Vault::open_in(&moved, Some(&data)).unwrap();
    assert_eq!(vault.rebound_from(), Some(old.as_path()));
    save(&vault, "b.md", b"b\n", "cli", None);
    Flusher::flush(&vault).unwrap();
    let top = git_external(
        &data.join("vaults/v-move/git"),
        &["rev-parse", "--show-toplevel"],
    );
    assert_eq!(git_worktree_path(&top), moved.canonicalize().unwrap());
    drop(vault);
    // The notice stays visible to later opens.
    let vault = Vault::open_in(&moved, Some(&data)).unwrap();
    assert_eq!(vault.rebound_from(), Some(old.as_path()));
}

#[test]
fn second_copy_with_the_same_vault_id_is_refused_not_taken_over() {
    let (tmp, root, data) = folder_vault("v-copy");
    drop(Vault::init_folder(&root, Some(&data)).unwrap());
    let copy = tmp.path().join("copy");
    std::fs::create_dir_all(copy.join(".wiki-island")).unwrap();
    std::fs::copy(
        root.join(".wiki-island/config.toml"),
        copy.join(".wiki-island/config.toml"),
    )
    .unwrap();
    let err = Vault::open_in(&copy, Some(&data)).unwrap_err();
    assert!(matches!(err, HistoryError::BoundElsewhere { .. }), "{err}");
    // The original still opens.
    assert!(Vault::open_in(&root, Some(&data)).is_ok());
}

#[test]
fn git_mode_vault_keeps_git_in_the_folder() {
    let (_dir, vault) = new_vault();
    assert_eq!(vault.storage_mode(), StorageMode::Git);
    assert_eq!(vault.git_dir(), vault.root().join(".git"));
    assert_eq!(vault.local_dir(), vault.root().join(".wiki-island/local"));
}
