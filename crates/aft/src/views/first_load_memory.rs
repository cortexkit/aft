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
        callgraph: crate::callgraph_extractor::PRODUCER.into(),
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

/// Times a fresh checkout's first bind when another checkout of the same
/// repository has already filled the stores its checkouts share: the case of
/// every new worker worktree. Copies `AFT_VIEW_FRESH_CORPUS` twice, binds the first copy
/// to warm the stores, then binds the second, edits one of its files and binds
/// it again on the same driver. Prints each bind's wall time, CPU time and
/// source work (walks, stats, reads, hashed bytes, attachments).
/// `AFT_VIEW_FRESH_DROP_ATTRIBUTES=1` deletes the corpus's `.gitattributes`
/// files from both copies before they are committed. Unix only: CPU time comes
/// from `getrusage`.
#[cfg(unix)]
#[test]
#[ignore = "fresh-checkout bind measurement; requires AFT_VIEW_FRESH_CORPUS"]
fn fresh_checkout_bind_after_a_warm_sibling() {
    let corpus = std::path::PathBuf::from(
        std::env::var_os("AFT_VIEW_FRESH_CORPUS").expect("AFT_VIEW_FRESH_CORPUS"),
    );
    let scratch = tempfile::tempdir().unwrap();
    let storage = scratch.path().join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    // Git before 2.42 cannot name its system and global attribute files, so
    // the checkout's conversion policy is not reusable, and the bind may not
    // skip reading a file on the strength of Git's index naming the same blob
    // a shared base recorded for it. AFT_VIEW_FRESH_MODERN_GIT=1 models a
    // newer Git that reports both files as absent.
    let _modern_git = std::env::var_os("AFT_VIEW_FRESH_MODERN_GIT")
        .map(|_| crate::views::verification::known_attribute_sources_for_test(scratch.path()));
    let checkout = |name: &str| {
        let root = scratch.path().join(name);
        let archive = std::process::Command::new("git")
            .args(["archive", "HEAD"])
            .current_dir(&corpus)
            .output()
            .unwrap();
        assert!(archive.status.success());
        std::fs::create_dir_all(&root).unwrap();
        let mut tar = std::process::Command::new("tar")
            .args(["-x", "-C"])
            .arg(&root)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(tar.stdin.as_mut().unwrap(), &archive.stdout).unwrap();
        assert!(tar.wait().unwrap().success());
        if std::env::var_os("AFT_VIEW_FRESH_DROP_ATTRIBUTES").is_some() {
            let listed = std::process::Command::new("git")
                .args(["ls-files", "-z", "*.gitattributes", ".gitattributes"])
                .current_dir(&corpus)
                .output()
                .unwrap();
            for path in listed.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                std::fs::remove_file(root.join(std::str::from_utf8(path).unwrap())).unwrap();
            }
        }
        for args in [
            &["init", "-q"][..],
            &["add", "-A"],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "commit",
                "-qm",
                "c",
            ],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap();
            assert!(status.success());
        }
        std::fs::canonicalize(root).unwrap()
    };
    let driver_for = |root: &std::path::Path, name: &str| {
        let bridge = Arc::new(CallgraphBridge::default());
        let policy = crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1 << 20,
        };
        let trigram = Arc::new(TrigramBridge::new(storage.clone(), policy.clone()));
        let producers = Producers {
            trigram: policy.fingerprint_hex(),
            semantic: None,
            callgraph: crate::callgraph_extractor::PRODUCER.into(),
        };
        let driver = CheckoutDriver::new_v3(
            &storage,
            "fresh",
            name,
            root,
            producers,
            Arc::new(ConfiguredMembershipWalker),
            vec![trigram, bridge.clone()],
        )
        .unwrap();
        Arc::new(driver.with_adapters(vec![bridge.adapter.clone()]))
    };
    let bind = |driver: &CheckoutDriver, root: &std::path::Path, name: &str| {
        let access = driver.access();
        let guard = crate::views::verification::BindGuard::capture(root, None).unwrap();
        let cpu = cpu_seconds();
        let start = std::time::Instant::now();
        let seed = driver.installed_snapshot().generation().clone();
        let (snapshot, revision) = driver.verified_bind(&access, seed).unwrap();
        let bound = start.elapsed();
        let work = driver.take_reconcile_work();
        driver.install(&access, &snapshot, revision).unwrap();
        eprintln!(
            "fresh_bind checkout={name} policy_reusable={} steady={} bind_s={:.3} cpu_s={:.3} work={work:?}",
            guard.policy.reusable,
            guard.steady_supported,
            bound.as_secs_f64(),
            cpu_seconds() - cpu,
        );
    };
    let first = checkout("first");
    let second = checkout("second");
    bind(&driver_for(&first, "first"), &first, "first");
    let driver = driver_for(&second, "second");
    bind(&driver, &second, "second");
    // One edited file, as after an agent's edit: the rebind should read only it.
    let edited = second.join("README.md");
    let mut bytes = std::fs::read(&edited).unwrap();
    bytes.extend_from_slice(b"\n# edited\n");
    std::fs::write(&edited, bytes).unwrap();
    bind(&driver, &second, "second-after-edit");
}

/// User plus system CPU time of this process, in seconds.
#[cfg(unix)]
fn cpu_seconds() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
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
