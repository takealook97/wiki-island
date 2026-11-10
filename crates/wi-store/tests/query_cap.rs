//! An over-cap text query is refused as `invalid-request` at the store and
//! never answered by the store's literal scan fallback.
mod common;

use common::{Fixture, write};
use wi_store::{Ingress, Query, SearchOptions, StoreError};

#[test]
fn an_over_cap_query_is_an_invalid_request_and_keeps_the_index() {
    let (_fx, vault) = Fixture::new();
    write(&vault, "a.md", "alpha beta\n", Ingress::Cli);
    let before = vault
        .search(&Query::text("alpha"), SearchOptions::default())
        .unwrap();
    assert!(!before.fallback);

    let over = Query::text(vec!["alpha"; 65].join(" "));
    let error = vault.search(&over, SearchOptions::default()).unwrap_err();
    assert!(error.is_query_too_long(), "{error}");
    assert_eq!(error.code(), "invalid-request");
    for bytes in [2049, 16_385, 100_000] {
        let over = Query::text("a".repeat(bytes));
        let error = vault.search(&over, SearchOptions::default()).unwrap_err();
        assert!(error.is_query_too_long(), "{error}");
        assert_eq!(error.code(), "invalid-request");
    }

    // The refusal did not switch the vault to the literal scan.
    let after = vault
        .search(&Query::text("alpha"), SearchOptions::default())
        .unwrap();
    assert!(!after.fallback);
    assert_eq!(after.hits.len(), 1);
    assert!(after.diagnostics.is_empty(), "{:?}", after.diagnostics);

    // Every other index error keeps its code.
    let other = StoreError::Index(wi_index::Error::InvalidVaultId);
    assert_eq!(other.code(), "index");
    assert!(!other.is_query_too_long());
}
