//! Offline, thread-CPU profiling of the real health rollup, not a parallel implementation.
use super::*;
pub(super) use crate::test_allocations::profile::Timer;

fn seed_inspect_findings(ctx: &AppContext, root: &Path, count: usize, tick: usize) {
    let manager = ctx.inspect_manager();
    let cache = manager
        .cache_for_paths(ctx.inspect_dir(), root.to_path_buf())
        .unwrap();
    let scope = crate::inspect::JobScope::for_project(root.to_path_buf());
    let key = crate::inspect::JobKey::for_category_scope(
        crate::inspect::InspectCategory::Diagnostics,
        &scope,
    );
    cache.store_aggregated(key, json!({
        "errors": 0, "warnings": count, "info": 0, "hints": 0,
        "items": (0..count).map(|index| json!({
            "file": format!("src/module_{}/file_{index}.ts", index % 64),
            "line": index + 1, "column": 1, "severity": "warning",
            "message": format!("diagnostic {index} at revision {tick}"),
            "source": "typescript", "complete": true,
        })).collect::<Vec<_>>(),
        "by_producer": { "typescript": { "errors": 0, "warnings": count, "info": 0, "hints": 0 } },
    })).unwrap();
    if tick == 0 {
        assert_eq!(manager.estimated_memory().counts["memory_aggregates"], 1);
    }
}
use crate::test_allocations::profile::{cpu_ns, STEPS};
use std::collections::BTreeMap;

#[test]
#[ignore = "offline idle-rollup CPU measurement with resident stale callgraph stores"]
fn bench_idle_resident_callgraph_rollup() {
    use super::super::test_support::{test_ctx, test_root};
    const ROOTS: usize = 111;
    const PATHS: usize = 200;
    const TICKS: usize = 20;
    let executor = Executor::new();
    let app = App::default_shared();
    let mut dirs = Vec::new();
    for index in 0..ROOTS {
        let (dir, root) = test_root(&format!("idle-callgraph-{index}"));
        let ctx = test_ctx();
        let files = (0..PATHS)
            .map(|file| {
                let path = root.as_path().join(format!("stale_{file}.ts"));
                std::fs::write(&path, "export function stale() {}\n").unwrap();
                path
            })
            .collect::<Vec<_>>();
        let store_dir = root.as_path().join(".callgraph-idle-bench");
        let writer = crate::callgraph_store::CallGraphStore::open(
            store_dir.clone(),
            root.as_path().to_path_buf(),
        )
        .unwrap();
        writer.cold_build(&files).unwrap();
        writer.mark_files_stale(&files).unwrap();
        drop(writer);
        *ctx.callgraph_store().write().unwrap() = Some(Arc::new(
            crate::callgraph_store::CallGraphStore::open_readonly(
                store_dir,
                root.as_path().to_path_buf(),
            )
            .unwrap()
            .unwrap(),
        ));
        assert!(executor.register_actor(root, ctx));
        dirs.push(dir);
    }
    let cache = HealthRollupCache::new();
    cache.refresh_for_test(&executor, &app);
    let before = crate::callgraph_store::STALE_PATH_CENSUS_BUILDS.with(std::cell::Cell::get);
    let mut cpu = 0;
    let mut elapsed = Duration::ZERO;
    let mut per_tick_us = Vec::new();
    for _ in 0..TICKS {
        // Real quiet intervals exercise the filesystem recheck backstop, not
        // just a burst of cache hits. Sleeping is outside the CPU measurement.
        std::thread::sleep(Duration::from_secs(3));
        let started = Instant::now();
        let started_cpu = cpu_ns();
        cache.refresh_for_test(&executor, &app);
        let tick_cpu = cpu_ns() - started_cpu;
        per_tick_us.push(tick_cpu / 1_000);
        cpu += tick_cpu;
        elapsed += started.elapsed();
        assert_eq!(cache.snapshot().0.metrics["root_count"], ROOTS);
    }
    println!("idle rollup roots={ROOTS} stale_paths_per_root={PATHS} ticks={TICKS} interval_s=3 cpu_us_per_tick={:.2} wall_us_per_tick={:.2} path_census_builds={} per_tick_cpu_us={per_tick_us:?}",
        cpu as f64 / TICKS as f64 / 1000., elapsed.as_secs_f64() * 1_000_000. / TICKS as f64,
        crate::callgraph_store::STALE_PATH_CENSUS_BUILDS.with(std::cell::Cell::get) - before);
}
