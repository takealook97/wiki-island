use std::sync::Barrier;
use tempfile::TempDir;
use wi_index::Index;

#[test]
fn concurrent_first_open_initializes_one_schema() {
    for iteration in 0..50 {
        let vault = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let database = cache.path().join("index.sqlite3");
        let barrier = Barrier::new(8);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        let index = Index::open_at(vault.path(), "race", &database)?;
                        // Keep all connections alive until every open completes.
                        Ok::<_, wi_index::Error>(index)
                    })
                })
                .collect();
            let indexes: Vec<_> = handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap()
                        .unwrap_or_else(|e| panic!("iteration {iteration}: {e}"))
                })
                .collect();
            for index in &indexes {
                assert_eq!(index.index_as_of().unwrap(), None);
            }
        });
    }
}
