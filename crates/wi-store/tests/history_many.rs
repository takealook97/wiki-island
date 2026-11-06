use wi_store::{Ingress, OpenOptions, Vault, WriteRequest};

#[test]
fn history_many_normalizes_paths_and_preserves_single_path_results() {
    let temp = tempfile::tempdir().unwrap();
    let (vault, _) = Vault::init_with(
        temp.path().join("vault"),
        OpenOptions {
            index_path: Some(temp.path().join("index.db")),
        },
    )
    .unwrap();
    for (path, content) in [("notes/a.md", "a"), ("b.md", "b")] {
        vault
            .write(WriteRequest {
                path: path.into(),
                content: content.as_bytes().to_vec(),
                expected_checksum: None,
                op_id: None,
                ingress: Ingress::Engine,
            })
            .unwrap();
    }
    vault.flush().unwrap();
    let absolute = vault.root().join("b.md").to_string_lossy().into_owned();
    let paths = [
        "./notes/a.md",
        absolute.as_str(),
        "missing.md",
        "./notes/a.md",
    ];
    let many = vault.history_many(&paths, None).unwrap();
    assert_eq!(
        many.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
        ["notes/a.md", "b.md", "missing.md", "notes/a.md"]
    );
    for ((_, entries), path) in many.iter().zip(paths) {
        assert_eq!(entries, &vault.history(path).unwrap());
    }
    assert!(vault.history_many(&["../outside.md"], None).is_err());
}
