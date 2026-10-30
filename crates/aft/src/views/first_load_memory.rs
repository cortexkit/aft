//! Isolated first-load heap and resident-set-size probe.
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
    #[cfg(target_os = "linux")]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
        eprintln!("first_load_memory rss_peak_kib={}", usage.ru_maxrss);
        if let Ok(limit) = std::env::var("AFT_VIEW_MEMORY_LIMIT_MIB") {
            let limit: i64 = limit.parse().unwrap();
            assert!(
                usage.ru_maxrss < limit * 1024,
                "first load used {} KiB resident memory; budget is {limit} MiB",
                usage.ru_maxrss
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn first_load_fixed_fixture_peak_rss() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    for file in 0..128 {
        let mut source = String::from("fn target() {}\n");
        for function in 0..500 {
            source.push_str(&format!(
                "fn caller_{function}() {{ target(); target(); target(); }}\n"
            ));
        }
        std::fs::write(root.path().join(format!("file_{file:03}.rs")), source).unwrap();
    }
    // A child isolates the process high-water mark from other unit tests.
    // The fixed 64,128-symbol fixture has a 384 MiB resident-memory budget.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "views::first_load::memory::first_load_memory_corpus",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AFT_VIEW_MEMORY_CORPUS", root.path())
        .env("AFT_VIEW_MEMORY_OUTPUT", storage.path())
        .env("AFT_VIEW_MEMORY_LIMIT_MIB", "384")
        .env("AFT_VIEW_PROFILE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
