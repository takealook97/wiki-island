//! A write reads the history manifest incrementally: the number of full
//! manifest parses, and of reads that compare the cached prefix with the
//! file, does not grow with the number of records it holds. Full parses
//! stay bounded even when something outside the engine moves the manifest's
//! file state after every write.

use std::path::Path;
use std::time::Duration;

use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

/// Move the modification time one second past its current value: no byte
/// changes, but the file state moves on every platform, as an indexer or
/// backup agent can do.
fn touch_metadata(path: &Path) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    let modified = file.metadata().unwrap().modified().unwrap();
    file.set_modified(modified + Duration::from_secs(1))
        .unwrap();
}

fn write_all(vault: &Vault, n: usize, disturb: bool) {
    let manifest = vault.history_vault().wi_dir().join("manifest.log");
    for i in 0..n {
        vault
            .write(WriteRequest {
                path: format!("bulk/note-{i}.md"),
                content: format!("# Note {i}\n\nBody {i}\n").into_bytes(),
                expected_checksum: None,
                op_id: None,
                ingress: Ingress::Import,
            })
            .unwrap();
        if disturb {
            touch_metadata(&manifest);
        }
    }
}

/// Full manifest parses and prefix comparisons on this thread while
/// writing `n` notes, moving the manifest's file state after every write
/// when `disturb` is set.
fn reads(n: usize, batch: bool, disturb: bool) -> (u64, u64) {
    let temp = tempfile::tempdir().unwrap();
    let (vault, _) = Vault::init_with(
        temp.path().join("vault"),
        OpenOptions {
            index_path: Some(temp.path().join("index.sqlite3")),
        },
    )
    .unwrap();
    let before = (
        wi_history::manifest_full_parses(),
        wi_history::manifest_prefix_compares(),
    );
    if batch {
        vault
            .import_batch(|| write_all(&vault, n, disturb))
            .unwrap();
    } else {
        write_all(&vault, n, disturb);
    }
    (
        wi_history::manifest_full_parses() - before.0,
        wi_history::manifest_prefix_compares() - before.1,
    )
}

fn assert_bounded(batch: bool) {
    let (small, small_compares) = reads(50, batch, false);
    let (large, large_compares) = reads(500, batch, false);
    let counts = format!(
        "batch {batch}: full parses 50 writes {small}, 500 writes {large}; \
         prefix compares 50 writes {small_compares}, 500 writes {large_compares}"
    );
    assert!(
        large <= small + 2,
        "full parses grow with the manifest ({counts})"
    );
    assert!(large < 500, "at least one full parse per write ({counts})");
    assert!(
        large_compares <= small_compares + 2,
        "the manifest is read again on every write ({counts})"
    );
}

#[test]
fn bulk_publish_does_not_reparse_history_manifest() {
    assert_bounded(true);
}

#[test]
fn single_publishes_do_not_reparse_history_manifest() {
    assert_bounded(false);
}

/// Prefix compares grow with the number of outside changes; full parses
/// must not. The lower bound on prefix compares proves the outside changes
/// really caused file-state misses.
#[test]
fn outside_metadata_changes_do_not_reparse_history_manifest() {
    let (small, small_compares) = reads(50, true, true);
    let (large, large_compares) = reads(500, true, true);
    let counts = format!(
        "full parses 50 writes {small}, 500 writes {large}; prefix compares \
         50 writes {small_compares}, 500 writes {large_compares}"
    );
    assert!(
        large <= small + 2,
        "full parses grow with outside metadata changes ({counts})"
    );
    assert!(
        large_compares >= 400,
        "outside metadata changes caused no file-state misses ({counts})"
    );
}
