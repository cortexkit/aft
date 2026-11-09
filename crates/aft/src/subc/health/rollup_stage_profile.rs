//! Offline per-stage CPU profile of the real health rollup over many bound
//! roots, the shape of a long-running daemon. Prints thread CPU and heap
//! allocations per rollup stage, summed over a run of simulated 3 s ticks.

use super::*;
use crate::subc::test_support::{test_ctx, test_root};

#[test]
#[ignore = "manual per-stage health rollup CPU profile over 80 roots"]
fn profile_rollup_stages_over_eighty_roots() {
    const ROOTS: usize = 80;
    const TICKS: u64 = 40;
    let executor = Executor::with_config(crate::executor::ExecutorConfig {
        pool_size: 1,
        read_cap: 1,
        actor_cap: 128,
        heavy_permits: 1,
        drr_quantum: 1,
    });
    let mut dirs = Vec::new();
    for index in 0..ROOTS {
        let (dir, root) = test_root(&format!("rollup-stages-{index:02}"));
        std::fs::write(dir.path().join("lib.rs"), "pub fn stage() {}\n").unwrap();
        dirs.push(dir);
        assert!(executor.register_actor(root, test_ctx()));
    }
    let app = crate::context::App::default_shared();
    let cache = HealthRollupCache::new();
    // Warm once outside the measured window: first-touch work is not steady state.
    cache.clock_ms.store(0, Ordering::Relaxed);
    cache.refresh_for_test(&executor, &app);
    crate::test_allocations::profile::STEPS
        .with(|steps| *steps.borrow_mut() = Some(Default::default()));
    let mut total_cpu_ns = 0u64;
    let mut slowest_ns = 0u64;
    let walks_before = crate::memory::allocator_snapshot_calls_for_test();
    let wall = Instant::now();
    for tick in 1..=TICKS {
        cache.clock_ms.store(tick * 3_000, Ordering::Relaxed);
        let started = crate::test_allocations::profile::cpu_ns();
        cache.refresh_for_test(&executor, &app);
        let spent = crate::test_allocations::profile::cpu_ns() - started;
        total_cpu_ns += spent;
        slowest_ns = slowest_ns.max(spent);
    }
    let wall = wall.elapsed();
    let walks = crate::memory::allocator_snapshot_calls_for_test() - walks_before;
    let steps = crate::test_allocations::profile::STEPS
        .with(|steps| steps.borrow_mut().take())
        .unwrap();
    let marked_ns: u64 = steps.values().map(|(cpu, _, _)| cpu).sum();
    let mut rows: Vec<_> = steps.into_iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    eprintln!(
        "rollup-stages roots={ROOTS} ticks={TICKS} census_builds={} allocator_walks={walks} wall_ms={} cpu_ms_total={:.1} cpu_ms_per_tick={:.2} slowest_tick_ms={:.2} unmarked_cpu_ms={:.1}",
        cache.census_builds_for_test(),
        wall.as_millis(),
        total_cpu_ns as f64 / 1e6,
        total_cpu_ns as f64 / 1e6 / TICKS as f64,
        slowest_ns as f64 / 1e6,
        total_cpu_ns.saturating_sub(marked_ns) as f64 / 1e6,
    );
    for (step, (cpu_ns, allocations, bytes)) in rows {
        eprintln!(
            "rollup-stage {step:<36} cpu_ms_total={:>8.1} share={:>5.1}% cpu_ms_per_tick={:>6.2} allocations_per_tick={:>8} bytes_per_tick={:>10}",
            cpu_ns as f64 / 1e6,
            cpu_ns as f64 * 100.0 / total_cpu_ns.max(1) as f64,
            cpu_ns as f64 / 1e6 / TICKS as f64,
            allocations as u64 / TICKS,
            bytes as u64 / TICKS,
        );
    }
    std::hint::black_box(dirs);
}

/// Thread CPU of each call the rollup's per-root loop makes, timed one call at
/// a time over the same 80 roots, to split the loop's stages further.
#[test]
#[ignore = "manual per-call health rollup root loop CPU profile over 80 roots"]
fn profile_rollup_root_loop_calls_over_eighty_roots() {
    use crate::test_allocations::profile::cpu_ns;
    const ROOTS: usize = 80;
    const TICKS: u64 = 40;
    let mut dirs = Vec::new();
    let mut actors = Vec::new();
    for index in 0..ROOTS {
        let (dir, root) = test_root(&format!("rollup-calls-{index:02}"));
        std::fs::write(dir.path().join("lib.rs"), "pub fn stage() {}\n").unwrap();
        dirs.push(dir);
        actors.push((root, test_ctx()));
    }
    let mut totals: std::collections::BTreeMap<&'static str, u64> = Default::default();
    let mut timed = |name: &'static str, started: u64| {
        *totals.entry(name).or_default() += cpu_ns() - started;
    };
    for _ in 0..=TICKS {
        let t = cpu_ns();
        let standing = standing_health_entries(&actors);
        timed("tick: standing_health_entries", t);
        let t = cpu_ns();
        let storage =
            crate::bash_background::storage_dir(actors[0].1.config().storage_dir.as_deref());
        timed("tick: bash_background::storage_dir", t);
        let t = cpu_ns();
        std::hint::black_box(crate::persisted_format::refusals_under(&storage));
        timed("tick: persisted_format::refusals_under", t);
        let t = cpu_ns();
        let cache_dir = crate::root_cache::cache_root(&actors[0].1.config());
        timed("tick: root_cache::cache_root", t);
        for (root_id, ctx) in &actors {
            let t = cpu_ns();
            let config = ctx.config();
            timed("root: ctx.config", t);
            let t = cpu_ns();
            let label = root_id.as_path().display().to_string();
            timed("root: root label", t);
            let t = cpu_ns();
            std::hint::black_box(standing.iter().position(|entry| {
                entry
                    .root_id
                    .as_ref()
                    .is_some_and(|entry_root| entry_root == root_id)
            }));
            timed("root: standing position", t);
            let t = cpu_ns();
            std::hint::black_box(ctx.memory_root_snapshot_and_rollup());
            timed("root: memory_root_snapshot_and_rollup", t);
            let t = cpu_ns();
            std::hint::black_box(crate::root_identity::key::artifact_cache_key_memoized_only(
                root_id.as_path(),
            ));
            timed("root: artifact_cache_key_memoized_only", t);
            let t = cpu_ns();
            std::hint::black_box(ctx.semantic_backend_health_snapshot());
            timed("root: semantic_backend_health_snapshot", t);
            let t = cpu_ns();
            std::hint::black_box(
                crate::semantic_index::embedding_backend_build_health(root_id.as_path()).filter(
                    |_| crate::synapse_embed::lane_pause_for_root(root_id.as_path()).is_none(),
                ),
            );
            timed("root: embedding_backend_build_health", t);
            let t = cpu_ns();
            let summary = ctx.try_health_summary_for_root(Some(&cache_dir));
            timed("root: try_health_summary_for_root", t);
            let t = cpu_ns();
            std::hint::black_box(summary.into_snapshot(root_id.as_path()));
            timed("root: health summary into_snapshot", t);
            let t = cpu_ns();
            std::hint::black_box((
                ctx.watcher_counters().snapshot(),
                config.bash.disclaim_privacy,
                ctx.database_runtime_failed(),
                ctx.try_lsp().map(|lsp| lsp.cached_start_failure_counts()),
            ));
            timed("root: watcher/database/lsp evidence", t);
            std::hint::black_box(label);
        }
    }
    let mut rows: Vec<_> = totals.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, ns) in rows {
        eprintln!(
            "rollup-call {name:<44} cpu_ms_per_tick={:>7.3} us_per_root={:>8.1}",
            ns as f64 / 1e6 / (TICKS + 1) as f64,
            ns as f64 / 1e3 / (TICKS + 1) as f64 / ROOTS as f64,
        );
    }
    std::hint::black_box(dirs);
}
