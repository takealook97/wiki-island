//! A write reads the history manifest incrementally: the number of full
//! manifest parses, and of reads that compare the cached prefix with the
//! file, does not grow with the number of records it holds.

use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

fn write_all(vault: &Vault, n: usize) {
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
    }
}

/// Full manifest parses and prefix comparisons on this thread while
/// writing `n` notes.
fn reads(n: usize, batch: bool) -> (u64, u64) {
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
        vault.import_batch(|| write_all(&vault, n)).unwrap();
    } else {
        write_all(&vault, n);
    }
    (
        wi_history::manifest_full_parses() - before.0,
        wi_history::manifest_prefix_compares() - before.1,
    )
}

fn assert_bounded(batch: bool) {
    let (small, small_compares) = reads(50, batch);
    let (large, large_compares) = reads(500, batch);
    assert!(
        large <= small + 2,
        "full parses grow with the manifest (batch {batch}): 50 writes {small}, 500 writes {large}"
    );
    assert!(large < 500, "at least one full parse per write: {large}");
    assert!(
        large_compares <= small_compares + 2,
        "the manifest is read again on every write (batch {batch}): \
         50 writes {small_compares}, 500 writes {large_compares}"
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
