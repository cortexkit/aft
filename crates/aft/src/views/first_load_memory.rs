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
    let legacy = std::env::var_os("AFT_VIEW_MEMORY_LEGACY").is_some();
    let producers = Producers {
        trigram: policy.fingerprint_hex(),
        semantic: None,
        callgraph: crate::views::callgraph::PRODUCER.into(),
    };
    let driver = if legacy {
        let registry = crate::views::registry::FamilyRegistry::open(&storage, "memory").unwrap();
        let owner = registry
            .register_view_with_storage(&storage, "worker", &root)
            .unwrap();
        CheckoutDriver::new(
            owner,
            producers,
            None,
            Arc::new(ConfiguredMembershipWalker),
            vec![trigram, bridge.clone()],
        )
    } else {
        CheckoutDriver::new_v3(
            &storage,
            "memory",
            "worker",
            &root,
            producers,
            Arc::new(ConfiguredMembershipWalker),
            vec![trigram, bridge.clone()],
        )
        .unwrap()
    };
    let driver = Arc::new(driver.with_adapters(vec![bridge.adapter.clone()]));
    let access = driver.access();
    let start = std::time::Instant::now();
    let snapshot = if legacy {
        let result = SiblingLoader::new(driver.clone(), vec![bridge.adapter.clone()])
            .load(&access)
            .unwrap();
        assert!(
            result.pending_planes.is_empty(),
            "{:?}",
            result.pending_planes
        );
        result.snapshot
    } else {
        let seed = driver.installed_snapshot().generation().clone();
        let (snapshot, revision) = driver.verified_bind(&access, seed).unwrap();
        driver.install(&access, &snapshot, revision).unwrap();
        snapshot
    };
    let reader = bridge.adapter.reader(&access, &snapshot).unwrap();
    assert!(reader.store.indexed_file_count().unwrap() > 0);
    crate::test_allocations::heap_marker("first_load", "ready");
    eprintln!(
        "first_load_memory database={}",
        reader.store.sqlite_path().display()
    );
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
        let rss_peak_kib =
            crate::test_allocations::rss_high_water_bytes().expect("Linux VmHWM") / 1024;
        eprintln!(
            "first_load_memory rss_peak_kib={rss_peak_kib} rusage_peak_kib={}",
            usage.ru_maxrss
        );
        if let Ok(limit) = std::env::var("AFT_VIEW_MEMORY_LIMIT_MIB") {
            let limit: u64 = limit.parse().unwrap();
            assert!(
                rss_peak_kib < limit.checked_mul(1024).expect("RSS budget fits KiB"),
                "first load used {} KiB resident memory; budget is {limit} MiB",
                rss_peak_kib
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
    // VmHWM measures the child's new address space; ru_maxrss can also include
    // the parent address space carried across the spawn's exec boundary.
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
        .env_remove("AFT_VIEW_MEMORY_LEGACY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
