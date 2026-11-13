//! Manual reproduction of health's memory census with retained, edited AFT engines.

use super::*;
use crate::data_layer::runtime::{
    memory_figure_walks_for_test, memory_profile_layer_for_test,
    with_memory_figure_baseline_for_test,
};
use crate::test_allocations::profile::{cpu_ns, STEPS};

fn copy_sources(source: &Path, root: &Path) {
    let tracked = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(source)
        .output()
        .unwrap();
    assert!(tracked.status.success());
    for path in tracked
        .stdout
        .split(|&byte| byte == 0)
        .filter(|p| !p.is_empty())
    {
        let relative = Path::new(std::str::from_utf8(path).unwrap());
        let original = source.join(relative);
        if !original.is_file() {
            continue;
        }
        let destination = root.join(relative);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::copy(original, destination).unwrap();
    }
}

fn bound_context(root: &Path, storage: &Path) -> Arc<AppContext> {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.to_path_buf());
    config.storage_dir = Some(storage.to_path_buf());
    config.indexes.semantic = false;
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    ctx.set_canonical_cache_root(root.to_path_buf());
    let checkout = ctx.data_checkout().expect("bind checkout");
    assert!(matches!(
        checkout.fresh_view(Duration::from_secs(300)),
        crate::data_layer::runtime::Fresh::Ready(_)
    ));
    checkout.wait_for_memory_profile_idle_for_test();
    ctx
}

#[test]
#[ignore = "manual optimized multi-checkout health memory CPU measurement"]
fn retained_aft_engines() {
    const ROOTS: usize = 40;
    const EDITING: usize = 3;
    assert!(!cfg!(debug_assertions), "use --profile ci-storm");
    let source = crate::test_support::manifest_dir().join("../..");
    let scratch = source.join("target/scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let dir = tempfile::tempdir_in(scratch).unwrap();
    let executor = Executor::with_config(crate::executor::ExecutorConfig {
        actor_cap: 64,
        ..Default::default()
    });
    // The production keeper deliberately evicts engines over 1 GiB. Keep all
    // forty real engines for this isolated profile rather than measuring empty slots.
    memory_profile_layer_for_test().set_engine_budget(16 << 30);
    memory_profile_layer_for_test().set_engine_idle(Duration::from_secs(3600));
    let app = crate::context::App::default_shared();
    let cache = HealthRollupCache::new();
    let mut contexts = Vec::new();
    let mut checkouts = Vec::new();
    for index in 0..ROOTS {
        let root = dir.path().join(format!("checkout-{index}"));
        copy_sources(&source, &root);
        eprintln!("memory fixture: binding checkout {index}");
        // Each context is bound below before it is registered as a daemon actor.
        let ctx = bound_context(&root, &dir.path().join("shared-store"));
        let checkout = ctx.data_checkout().expect("bound checkout");
        let paths = checkout.checkout().view().unwrap().state().paths().len();
        assert!(
            paths >= 6000,
            "fixture must contain the complete AFT repository"
        );
        // A broad first edit populates resolver caches; later ticks change only
        // one of these files, as a rebase does while other engines remain kept.
        let seeds = checkout
            .checkout()
            .view()
            .unwrap()
            .state()
            .paths()
            .iter()
            .filter(|entry| {
                entry.path.starts_with(b"crates/aft/src/") && entry.path.ends_with(b".rs")
            })
            .take(40)
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(seeds.len(), 40);
        for (seed, relative) in seeds.iter().enumerate() {
            let path = root.join(std::str::from_utf8(relative).unwrap());
            let mut bytes = std::fs::read(&path).unwrap();
            // Siblings may reuse a publication that already includes another
            // root's edits. Unique declarations ensure this refresh is real.
            bytes.extend_from_slice(format!("\npub fn health_memory_seed_{index}_{seed}() {{ let _ = crate::context::AppContext::new; }}\n").as_bytes());
            std::fs::write(path, bytes).unwrap();
        }
        checkout.checkout().record_paths(&seeds).unwrap();
        checkout.checkout().refresh_callgraph().unwrap();
        assert!(
            checkout.checkout().engine_memory_parts().is_some(),
            "engine is retained"
        );
        let parts = checkout.checkout().engine_memory_parts().unwrap();
        let bytes: usize = parts.iter().map(|(_, bytes)| bytes).sum();
        let bound = parts
            .iter()
            .find(|(name, _)| *name == "engine_bound")
            .unwrap()
            .1;
        println!("MEMORY_FIXTURE root={index} paths={paths} bytes={bytes} parts={parts:?}");
        assert!(
            bytes >= 100_000_000 && bound >= 20_000_000,
            "root {index} must retain a realistic engine, not a lazy or empty fixture: {parts:?}"
        );
        assert!(executor.register_actor(ProjectRootId::from_path(&root).unwrap(), Arc::clone(&ctx)));
        contexts.push(ctx);
        checkouts.push(checkout);
    }
    for checkout in &checkouts {
        assert!(
            checkout.checkout().engine_memory_parts().is_some(),
            "every engine must still be retained"
        );
    }
    let start = Instant::now();
    for (mode_index, (mode, baseline)) in
        [("before", true), ("after", false)].into_iter().enumerate()
    {
        with_memory_figure_baseline_for_test(baseline, || {
            for (phase_index, (phase, editing)) in
                [("rest", false), ("edits", true)].into_iter().enumerate()
            {
                let epoch =
                    start + Duration::from_secs((mode_index * 240 + phase_index * 120) as u64);
                // Warm initial publication and memory figures outside the steady-state window.
                crate::context::with_memory_walk_clock_for_test(epoch, || {
                    cache.refresh_for_test(&executor, &app);
                });
                let mut total_cpu = 0;
                let mut walks = 0;
                let mut walk_cpu = [0u64; ROOTS];
                let mut walk_counts = [0usize; ROOTS];
                STEPS.with(|steps| *steps.borrow_mut() = Some(Default::default()));
                for tick in 1..=20 {
                    if editing {
                        // Exclude engine-keeper censuses done by refresh from rollup component CPU.
                        let steps = STEPS.with(|steps| steps.borrow_mut().take());
                        for checkout in checkouts.iter().take(EDITING) {
                            let relative = b"crates/aft/src/lib.rs".to_vec();
                            let path = checkout
                                .root()
                                .join(std::str::from_utf8(&relative).unwrap());
                            let mut bytes = std::fs::read(&path).unwrap();
                            bytes.extend_from_slice(
                                format!("\npub fn health_memory_profile_{mode}_{tick}() {{}}\n")
                                    .as_bytes(),
                            );
                            std::fs::write(path, bytes).unwrap();
                            checkout.checkout().record_paths(&[relative]).unwrap();
                            checkout.checkout().refresh_callgraph().unwrap();
                        }
                        STEPS.with(|held| *held.borrow_mut() = steps);
                    }
                    let now = epoch + Duration::from_secs(tick * 3);
                    crate::context::with_memory_walk_clock_for_test(now, || {
                        // Measure the census separately per root, then the full rollup
                        // on the same thread. The latter reuses those exact figures.
                        let mut root_cpu = 0;
                        for (index, ctx) in contexts.iter().enumerate() {
                            let before_walks = memory_figure_walks_for_test();
                            let before_cpu = cpu_ns();
                            std::hint::black_box(crate::data_layer::surfaces::memory_estimate(ctx));
                            let elapsed = cpu_ns() - before_cpu;
                            root_cpu += elapsed;
                            let count = memory_figure_walks_for_test() - before_walks;
                            walks += count;
                            walk_counts[index] += count;
                            if count != 0 {
                                walk_cpu[index] += elapsed;
                            }
                        }
                        cache.clock_ms.store(tick * 3_000, Ordering::Relaxed);
                        let before_cpu = cpu_ns();
                        cache.refresh_for_test(&executor, &app);
                        total_cpu += root_cpu + cpu_ns() - before_cpu;
                    });
                }
                let steps = STEPS.with(|steps| steps.borrow_mut().take().unwrap());
                for index in 0..ROOTS {
                    println!(
                "MEMORY_WALK mode={mode} phase={phase} root={index} walks_per_minute={} cpu_ms_per_walk={:.3}",
                walk_counts[index],
                walk_cpu[index] as f64 / walk_counts[index].max(1) as f64 / 1e6
            );
                }
                println!("MEMORY_ROLLUP mode={mode} phase={phase} roots={ROOTS} editing_roots={} simulated_seconds=60 asks=20 walks_per_minute={walks} cpu_ms_per_minute={:.3} core_percent={:.3}", if editing {EDITING} else {0}, total_cpu as f64 / 1e6, total_cpu as f64 / 6e8);
                for (part, (cpu, _, _)) in steps
                    .into_iter()
                    .filter(|(part, _)| part.starts_with("engine /"))
                {
                    println!(
                        "MEMORY_PART mode={mode} phase={phase} part={part:?} cpu_ms={:.3}",
                        cpu as f64 / 1e6
                    );
                }
            }
        });
    }
}
