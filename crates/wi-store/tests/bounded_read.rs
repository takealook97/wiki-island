//! Reads a caller aims at a file are bounded by `max_note_bytes`.
mod common;

use common::Fixture;
use wi_store::{StoreError, Vault};

const LIMIT: usize = 64;

/// A vault whose config sets `max_note_bytes = limit`, reopened so the
/// setting is loaded.
fn vault_with_limit(fx: &Fixture, limit: usize) -> Vault {
    let config = fx.root_input().join(".wiki-island/config.toml");
    let mut text: String = std::fs::read_to_string(&config)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("max_note_bytes"))
        .map(|line| format!("{line}\n"))
        .collect();
    text.push_str(&format!("max_note_bytes = {limit}\n"));
    std::fs::write(&config, text).unwrap();
    fx.open()
}

#[test]
fn read_bounded_refuses_a_file_one_byte_over_the_limit() {
    let (fx, vault) = Fixture::new();
    drop(vault);
    let vault = vault_with_limit(&fx, LIMIT);
    std::fs::create_dir_all(vault.root().join("files")).unwrap();
    std::fs::write(vault.root().join("files/over.bin"), vec![b'x'; LIMIT + 1]).unwrap();
    std::fs::write(vault.root().join("over.md"), vec![b'a'; LIMIT + 1]).unwrap();
    for path in ["files/over.bin", "over.md"] {
        match vault.read_bounded(path).unwrap_err() {
            StoreError::TooLarge {
                path: named,
                size,
                limit,
            } => {
                assert_eq!(named, path);
                assert_eq!(size, LIMIT as u64 + 1);
                assert_eq!(limit, LIMIT as u64);
            }
            other => panic!("{other}"),
        }
        assert_eq!(
            StoreError::TooLarge {
                path: path.into(),
                size: 0,
                limit: 0
            }
            .code(),
            "too-large"
        );
        // The whole-file read used by write-path steps is unchanged.
        assert_eq!(vault.read(path).unwrap().bytes.len(), LIMIT + 1);
    }
}

#[test]
fn read_bounded_reads_a_file_exactly_at_the_limit() {
    let (fx, vault) = Fixture::new();
    drop(vault);
    let vault = vault_with_limit(&fx, LIMIT);
    let body = vec![b'b'; LIMIT];
    std::fs::write(vault.root().join("at.md"), &body).unwrap();
    let bounded = vault.read_bounded("at.md").unwrap();
    let whole = vault.read("at.md").unwrap();
    assert_eq!(bounded.bytes, body);
    assert_eq!(bounded.checksum, whole.checksum);
    assert_eq!(bounded.path, "at.md");
}

#[test]
fn a_raised_max_note_bytes_allows_the_larger_file() {
    let (fx, vault) = Fixture::new();
    drop(vault);
    std::fs::write(fx.root_input().join("big.md"), vec![b'c'; LIMIT + 1]).unwrap();
    let vault = vault_with_limit(&fx, LIMIT);
    assert!(matches!(
        vault.read_bounded("big.md"),
        Err(StoreError::TooLarge { .. })
    ));
    drop(vault);
    let vault = vault_with_limit(&fx, LIMIT * 2);
    assert_eq!(vault.read_bounded("big.md").unwrap().bytes.len(), LIMIT + 1);
}

#[test]
fn read_bounded_keeps_not_found_and_default_limit() {
    let (_fx, vault) = Fixture::new();
    assert!(matches!(
        vault.read_bounded("missing.md"),
        Err(StoreError::NotFound { .. })
    ));
    // The default limit (8 MiB) admits ordinary notes.
    std::fs::write(vault.root().join("small.md"), b"# Small\n").unwrap();
    assert_eq!(vault.read_bounded("small.md").unwrap().bytes, b"# Small\n");
}

#[test]
fn require_readable_succeeds_on_a_file_above_the_limit_without_reading_it() {
    let (fx, vault) = Fixture::new();
    drop(vault);
    let vault = vault_with_limit(&fx, LIMIT);
    std::fs::write(vault.root().join("big.bin"), vec![b'd'; LIMIT * 4]).unwrap();
    vault.require_readable("big.bin").unwrap();
    assert!(matches!(
        vault.read_bounded("big.bin"),
        Err(StoreError::TooLarge { .. })
    ));
}

#[test]
fn require_readable_reports_a_missing_file_as_not_found() {
    let (_fx, vault) = Fixture::new();
    match vault.require_readable("gone/missing.md").unwrap_err() {
        StoreError::NotFound { path } => assert_eq!(path, "gone/missing.md"),
        other => panic!("{other}"),
    }
    match vault.read("gone/missing.md").unwrap_err() {
        StoreError::NotFound { path } => assert_eq!(path, "gone/missing.md"),
        other => panic!("{other}"),
    }
}

#[cfg(unix)]
#[test]
fn require_readable_refuses_a_symlink_and_a_directory_like_read() {
    let (_fx, vault) = Fixture::new();
    std::fs::write(vault.root().join("real.md"), b"# Real\n").unwrap();
    std::os::unix::fs::symlink("real.md", vault.root().join("link.md")).unwrap();
    std::fs::create_dir(vault.root().join("folder")).unwrap();
    for path in ["link.md", "folder"] {
        let read = vault.read(path).unwrap_err().to_string();
        let required = vault.require_readable(path).unwrap_err().to_string();
        assert_eq!(required, read, "{path}");
    }
}
