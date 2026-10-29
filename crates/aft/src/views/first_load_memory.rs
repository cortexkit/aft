//! Isolated first-load heap probe; measure resident set size with `/usr/bin/time -v`.
//! Run only this ignored test in its process.
use super::*;
use crate::views::contracts::PlaneLoader;

#[test]
#[ignore = "whole-checkout first-load measurement; requires AFT_VIEW_MEMORY_CORPUS"]
fn first_load_memory_corpus() {
    let root = std::path::PathBuf::from(
        std::env::var_os("AFT_VIEW_MEMORY_CORPUS").expect("AFT_VIEW_MEMORY_CORPUS"),
    );
    let storage = std::path::PathBuf::from(
        std::env::var_os("AFT_VIEW_MEMORY_OUTPUT").expect("AFT_VIEW_MEMORY_OUTPUT"),
    );
    std::fs::create_dir_all(&storage).unwrap();
    let bridge = Arc::new(CallgraphBridge::default());
    let policy = crate::blob_store::v2::TrigramPolicy {
        max_file_size: 1 << 20,
    };
    let trigram = Arc::new(TrigramBridge::new(storage.clone(), policy.clone()));
    let registry = crate::views::registry::FamilyRegistry::open(&storage, "memory").unwrap();
    let owner = registry
        .register_view_with_storage(&storage, "worker", &root)
        .unwrap();
    let driver = Arc::new(
        CheckoutDriver::new(
            owner,
            Producers {
                trigram: policy.fingerprint_hex(),
                semantic: None,
                callgraph: crate::views::callgraph::PRODUCER.into(),
            },
            None,
            Arc::new(ConfiguredMembershipWalker),
            vec![trigram, bridge.clone()],
        )
        .with_adapters(vec![bridge.adapter.clone()]),
    );
    let access = driver.access();
    let loader = SiblingLoader::new(driver.clone(), vec![bridge.adapter.clone()]);
    let start = std::time::Instant::now();
    let result = loader.load(&access).unwrap();
    assert!(
        result.pending_planes.is_empty(),
        "{:?}",
        result.pending_planes
    );
    let reader = bridge.adapter.reader(&access, &result.snapshot).unwrap();
    assert!(reader.store.indexed_file_count().unwrap() > 0);
    crate::test_allocations::heap_marker("first_load", "ready");
    eprintln!(
        "first_load_memory wall_s={:.3} files={} storage={}",
        start.elapsed().as_secs_f64(),
        reader.store.indexed_file_count().unwrap(),
        storage.display()
    );
}
