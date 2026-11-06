//! An ignored directory (`name/` in `.gitignore`) is pruned before it is
//! read, so an unreadable one never fails a scan or an index walk.

mod common;

use common::Fixture;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use wi_index::Query;
use wi_store::{SearchOptions, Vault};

fn hits(vault: &Vault, word: &str) -> Vec<String> {
    vault
        .search(&Query::text(word), SearchOptions::default())
        .unwrap()
        .hits
        .iter()
        .map(|h| h.rel_path.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn unreadable_ignored_directory_is_pruned_by_scans_and_index_walks() {
    let (_fx, vault) = Fixture::new();
    let root = vault.root().to_path_buf();
    let gitignore = root.join(".gitignore");
    let mut rules = fs::read_to_string(&gitignore).unwrap_or_default();
    rules.push_str("\nignored/\n");
    fs::write(&gitignore, rules).unwrap();
    fs::write(root.join("kept.md"), "keptword").unwrap();
    let hidden = root.join("ignored");
    fs::create_dir(&hidden).unwrap();
    fs::write(hidden.join("x.md"), "hiddenword").unwrap();
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    let scan = vault.reconcile();
    let sync = vault.catch_up();
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
    let scan = scan.unwrap();
    assert!(scan.failed.is_empty(), "{:?}", scan.failed);
    assert!(
        scan.ingested.iter().all(|i| !i.path.starts_with("ignored")),
        "{scan:?}"
    );
    let sync = sync.unwrap();
    assert!(sync.skipped.is_empty(), "{:?}", sync.skipped);
    assert!(sync.diagnostics.is_empty(), "{:?}", sync.diagnostics);
    assert!(wi_store::ignored_dir(
        &root,
        std::path::Path::new("ignored")
    ));
    assert_eq!(hits(&vault, "keptword"), vec!["kept.md".to_owned()]);
    assert!(hits(&vault, "hiddenword").is_empty());
}
