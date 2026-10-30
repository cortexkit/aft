//! Cost of the idle-root reaper's sweep, which runs every drain tick
//! (`DRAIN_TICK_PERIOD`) on the subc frame loop that also reads and writes
//! every client frame.
//!
//! An unbound root past its idle TTL that an artifact blocker keeps resident
//! (a standing root, running bash, an unpersisted search delta) has its
//! pending watcher-derived path sets taken and put back on every sweep. These
//! tests pin what that round trip costs.

use super::test_support::{test_ctx, test_root};
use super::*;
use crate::context::pending_restore_work;
use std::collections::BTreeSet;

type PendingSnapshot = (
    BTreeSet<PathBuf>,
    BTreeSet<PathBuf>,
    BTreeSet<PathBuf>,
    BTreeSet<PathBuf>,
    bool,
);

/// A small deterministic generator, so a failing sequence replays from its
/// seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// The same snapshot with every path made relative to `root`, so two worlds
/// built under different temporary directories compare equal.
fn relative_snapshot(root: &Path, snapshot: PendingSnapshot) -> PendingSnapshot {
    let strip = |set: BTreeSet<PathBuf>| {
        set.into_iter()
            .map(|path| {
                path.strip_prefix(root)
                    .map(Path::to_path_buf)
                    .unwrap_or(path)
            })
            .collect::<BTreeSet<_>>()
    };
    (
        strip(snapshot.0),
        strip(snapshot.1),
        strip(snapshot.2),
        strip(snapshot.3),
        snapshot.4,
    )
}

/// Unbound, quiesced, TTL-aged roots, each registered with one executor and
/// kept resident by the standing-root artifact blocker.
struct BlockedUnboundRoots {
    _dirs: Vec<tempfile::TempDir>,
    contexts: Vec<(ProjectRootId, Arc<AppContext>)>,
    executor: Arc<Executor>,
    live_roots: HashMap<ProjectRootId, RootMeta>,
}

impl BlockedUnboundRoots {
    fn new(count: usize) -> Self {
        let executor = Arc::new(Executor::new());
        let mut dirs = Vec::with_capacity(count);
        let mut contexts = Vec::with_capacity(count);
        let mut live_roots = HashMap::with_capacity(count);
        for index in 0..count {
            let (dir, root) = test_root(&format!("idle-reap-frame-loop-{index}-"));
            let ctx = test_ctx();
            assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
            ctx.mark_subc_unbound();
            ctx.set_standing_artifact_exempt(true);
            let mut meta = RootMeta::new(Instant::now());
            meta.unbound_quiesced = true;
            live_roots.insert(root.clone(), meta);
            dirs.push(dir);
            contexts.push((root, ctx));
        }
        Self {
            _dirs: dirs,
            contexts,
            executor,
            live_roots,
        }
    }

    /// Replace every root's pending sets with `per_set` paths in each of the
    /// search, callgraph, Tier-2 and semantic sets. The paths share the root
    /// prefix and nest a few directories deep, as watcher events do, so
    /// ordering them compares several leading components first.
    fn fill_pending(&self, per_set: usize) {
        for (root, ctx) in &self.contexts {
            ctx.clear_pending_index_updates();
            let paths = || {
                (0..per_set).map(|index| {
                    root.as_path().join(format!(
                        "src/module_{}/submodule_{}/file_{index}.rs",
                        index % 17,
                        index % 5
                    ))
                })
            };
            ctx.add_pending_search_index_paths(paths());
            ctx.add_pending_callgraph_store_paths(paths());
            ctx.add_pending_tier2_paths(paths());
            ctx.add_pending_semantic_index_paths(paths());
        }
    }

    /// One sweep as the frame loop runs it: past every root's TTL, with no
    /// bound routes, no binds in flight and no new presence observation.
    fn sweep(&mut self, metrics: &DispatchPathMetrics) -> IdleReapOutcome {
        reap_idle_roots_with_presence(
            Instant::now() + IDLE_ROOT_TTL + Duration::from_secs(1),
            &mut self.live_roots,
            &HashMap::new(),
            &HashMap::new(),
            &self.executor,
            metrics,
            &RootPresence::default(),
        )
    }

    fn pending_snapshots(&self) -> Vec<PendingSnapshot> {
        self.contexts
            .iter()
            .map(|(_, ctx)| ctx.pending_reconciliation_snapshot_for_test())
            .collect()
    }
}

/// Each registered actor holds a file descriptor until it is removed, and
/// these fixtures register many; without this, running them beside the rest
/// of the suite can exhaust the process's descriptor limit.
impl Drop for BlockedUnboundRoots {
    fn drop(&mut self) {
        for (root, _) in &self.contexts {
            self.executor.remove_actor(root);
        }
    }
}

/// Per-sweep wall time of the reaper at the daemon's observed scale (about
/// 170 roots). Ignored by default because it measures time; run it with
/// `cargo test -p agent-file-tools --lib -- --ignored --nocapture
/// idle_reap_sweep_cost_at_170_roots`.
#[test]
#[ignore = "benchmark: prints per-sweep timings"]
fn idle_reap_sweep_cost_at_170_roots() {
    const ROOTS: usize = 170;
    const SWEEPS: usize = 21;
    let mut fixture = BlockedUnboundRoots::new(ROOTS);
    let metrics = DispatchPathMetrics::new();
    for per_set in [0, 50, 500, 2_000] {
        fixture.fill_pending(per_set);
        let mut samples = Vec::with_capacity(SWEEPS);
        for _ in 0..SWEEPS {
            let started = Instant::now();
            let outcome = fixture.sweep(&metrics);
            samples.push(started.elapsed());
            assert_eq!(
                outcome.evicted, 0,
                "the standing blocker must hold every root"
            );
        }
        samples.sort();
        println!(
            "idle reap sweep: roots={ROOTS} pending_paths_per_root={} median={:?} min={:?} max={:?}",
            per_set * 4,
            samples[SWEEPS / 2],
            samples[0],
            samples[SWEEPS - 1],
        );
    }
}

/// Every sweep takes and restores each blocked root's pending sets. Moving a
/// taken set back must not depend on how many roots are blocked or how many
/// paths each holds: the path-by-path re-insertion it replaced compared whole
/// paths for every pending path of every blocked root, four times a second.
#[test]
fn reap_sweep_restore_work_does_not_grow_with_roots_or_pending_paths() {
    for (roots, per_set) in [(1, 8), (4, 64), (16, 256)] {
        let metrics = DispatchPathMetrics::new();
        let mut fixture = BlockedUnboundRoots::new(roots);
        fixture.fill_pending(per_set);
        let before = fixture.pending_snapshots();
        pending_restore_work::take_merged();

        for _ in 0..3 {
            assert_eq!(fixture.sweep(&metrics).evicted, 0);
        }
        let merged = pending_restore_work::take_merged();

        // Every root reached the take-and-restore round trip: the census
        // names the standing blocker, which is only consulted after the take.
        let census = metrics.reap_census_for_test();
        assert_eq!(census["unbound_retained"], roots, "{census}");
        assert_eq!(
            census["unbound_blockers"]["standing_root"], roots,
            "{census}"
        );
        assert_eq!(
            fixture.pending_snapshots(),
            before,
            "a blocked sweep must put back every pending path it took"
        );
        assert_eq!(
            merged,
            0,
            "{roots} blocked root(s) with {per_set} paths per set made the sweep merge {merged} paths one by one"
        );

        // Control: the path-by-path re-insertion does grow with both, so the
        // counter above would have seen it.
        pending_restore_work::with_legacy_restore(|| fixture.sweep(&metrics));
        assert_eq!(pending_restore_work::take_merged(), roots * per_set * 4);
        assert_eq!(fixture.pending_snapshots(), before);
    }
}

/// Restoring a taken pending state, with or without paths recorded while it
/// was out, leaves exactly the sets the path-by-path re-insertion leaves.
#[test]
fn restoring_taken_pending_state_matches_path_by_path_reinsertion() {
    let new_ctx = test_ctx();
    let legacy_ctx = test_ctx();
    let random_paths = |rng: &mut Rng, max: u64| {
        (0..rng.below(max + 1))
            .map(|_| {
                PathBuf::from(format!(
                    "/work/project/src/dir_{}/file_{}.rs",
                    rng.below(6),
                    rng.below(48)
                ))
            })
            .collect::<Vec<_>>()
    };
    for seed in 1..=96 {
        let mut rng = Rng::new(seed);
        let base: [Vec<PathBuf>; 4] = std::array::from_fn(|_| random_paths(&mut rng, 40));
        let base_corpus = rng.chance(50);
        let concurrent: [Vec<PathBuf>; 4] = std::array::from_fn(|set| {
            // Seeds where nothing arrives while the state is out exercise the
            // whole-set move; the rest exercise the merge.
            if seed % 3 == 0 || set == rng.below(4) as usize {
                Vec::new()
            } else {
                random_paths(&mut rng, 20)
            }
        });
        let concurrent_corpus = rng.chance(30);
        let blocking_restore = rng.chance(50);
        let nothing_concurrent = concurrent.iter().all(Vec::is_empty);

        let mut snapshots = Vec::new();
        for (ctx, legacy) in [(&new_ctx, false), (&legacy_ctx, true)] {
            ctx.clear_pending_index_updates();
            let fill = |paths: &[Vec<PathBuf>; 4], corpus: bool| {
                ctx.add_pending_search_index_paths(paths[0].clone());
                ctx.add_pending_callgraph_store_paths(paths[1].clone());
                ctx.add_pending_tier2_paths(paths[2].clone());
                ctx.add_pending_semantic_index_paths(paths[3].clone());
                if corpus {
                    ctx.mark_pending_semantic_corpus_refresh();
                }
            };
            fill(&base, base_corpus);
            let taken = ctx
                .try_take_pending_reconciliation_state()
                .expect("uncontended take");
            fill(&concurrent, concurrent_corpus);
            pending_restore_work::take_merged();
            let restore = || {
                if blocking_restore {
                    ctx.restore_pending_reconciliation_state(taken);
                } else {
                    ctx.restore_pending_reconciliation_state_without_waiting(taken);
                }
            };
            if legacy {
                pending_restore_work::with_legacy_restore(restore);
            } else {
                restore();
                if nothing_concurrent {
                    assert_eq!(pending_restore_work::take_merged(), 0, "seed {seed}");
                }
            }
            snapshots.push(ctx.pending_reconciliation_snapshot_for_test());
        }
        assert_eq!(snapshots[0], snapshots[1], "seed {seed}");
        let expected_search = base[0]
            .iter()
            .chain(&concurrent[0])
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(snapshots[0].0, expected_search, "seed {seed}");
        assert_eq!(
            snapshots[0].4,
            base_corpus || concurrent_corpus,
            "seed {seed}"
        );
    }
}

/// One copy of a randomized reaper scenario. Two copies built from the same
/// seed see the same roots, routes, blockers, pending paths and presence
/// probes, so their reap decisions must match sweep for sweep.
struct ReapWorld {
    _dirs: Vec<tempfile::TempDir>,
    roots: Vec<ProjectRootId>,
    contexts: Vec<Arc<AppContext>>,
    executor: Arc<Executor>,
    live_roots: HashMap<ProjectRootId, RootMeta>,
    root_channels: HashMap<ProjectRootId, HashSet<RouteChannel>>,
    metrics: DispatchPathMetrics,
    legacy_restore: bool,
}

/// What one sweep decided, with roots named by index.
#[derive(Debug, PartialEq)]
struct SweepRecord {
    evicted: usize,
    forgotten: Vec<usize>,
    census: serde_json::Value,
    metas: Vec<(usize, u8, u64, bool, Option<&'static str>, bool)>,
    pending: Vec<(usize, PendingSnapshot)>,
}

impl ReapWorld {
    fn new(roots: usize, legacy_restore: bool) -> Self {
        let executor = Arc::new(Executor::new());
        let mut world = Self {
            _dirs: Vec::new(),
            roots: Vec::new(),
            contexts: Vec::new(),
            executor,
            live_roots: HashMap::new(),
            root_channels: HashMap::new(),
            metrics: DispatchPathMetrics::new(),
            legacy_restore,
        };
        for index in 0..roots {
            let (dir, root) = test_root(&format!("idle-reap-differential-{index}-"));
            let ctx = test_ctx();
            assert!(world
                .executor
                .register_actor(root.clone(), Arc::clone(&ctx)));
            ctx.mark_subc_unbound();
            world._dirs.push(dir);
            world.roots.push(root);
            world.contexts.push(ctx);
        }
        world
    }

    fn index_of(&self, root: &ProjectRootId) -> usize {
        self.roots.iter().position(|known| known == root).unwrap()
    }

    fn set_meta(&mut self, index: usize, last_touched: Instant, quiesced: bool) {
        let mut meta = RootMeta::new(last_touched);
        meta.unbound_quiesced = quiesced;
        self.live_roots.insert(self.roots[index].clone(), meta);
    }

    fn set_bound(&mut self, index: usize, bound: bool) {
        let root = self.roots[index].clone();
        if bound {
            self.root_channels.insert(
                root,
                HashSet::from([RouteChannel {
                    channel: index as u16,
                    epoch: 1,
                }]),
            );
        } else {
            self.root_channels.remove(&root);
        }
    }

    fn add_pending(&self, index: usize, set: u64, relative: &[String], corpus: bool) {
        let ctx = &self.contexts[index];
        let paths = relative
            .iter()
            .map(|relative| self.roots[index].as_path().join(relative))
            .collect::<Vec<_>>();
        match set {
            0 => ctx.add_pending_search_index_paths(paths),
            1 => ctx.add_pending_callgraph_store_paths(paths),
            2 => ctx.add_pending_tier2_paths(paths),
            _ => ctx.add_pending_semantic_index_paths(paths),
        }
        if corpus {
            ctx.mark_pending_semantic_corpus_refresh();
        }
    }

    fn sweep(&mut self, now: Instant, generation: u64, absent: &[usize]) -> SweepRecord {
        let mut presence = RootPresence {
            generation,
            ..RootPresence::default()
        };
        for (index, root) in self.roots.iter().enumerate() {
            if self.live_roots.contains_key(root) {
                presence.probed.insert(root.clone());
                if absent.contains(&index) {
                    presence.absent.insert(root.clone());
                }
            }
        }
        let mut sweep = || {
            reap_idle_roots_with_presence(
                now,
                &mut self.live_roots,
                &HashMap::new(),
                &self.root_channels,
                &self.executor,
                &self.metrics,
                &presence,
            )
        };
        let outcome = if self.legacy_restore {
            pending_restore_work::with_legacy_restore(sweep)
        } else {
            sweep()
        };
        let mut forgotten = outcome
            .forgotten_deleted_roots
            .iter()
            .map(|root| self.index_of(root))
            .collect::<Vec<_>>();
        forgotten.sort_unstable();
        let mut metas = self
            .live_roots
            .iter()
            .map(|(root, meta)| {
                (
                    self.index_of(root),
                    meta.consecutive_missing_sweeps,
                    meta.presence_generation_seen,
                    meta.idle_artifacts_evicted,
                    meta.ttl_retention_logged,
                    meta.teardown_persist_submitted_at.is_some(),
                )
            })
            .collect::<Vec<_>>();
        metas.sort_unstable_by_key(|meta| meta.0);
        let pending = self
            .roots
            .iter()
            .zip(&self.contexts)
            .enumerate()
            .map(|(index, (root, ctx))| {
                (
                    index,
                    relative_snapshot(
                        root.as_path(),
                        ctx.pending_reconciliation_snapshot_for_test(),
                    ),
                )
            })
            .collect();
        SweepRecord {
            evicted: outcome.evicted,
            forgotten,
            census: self.metrics.reap_census_for_test(),
            metas,
            pending,
        }
    }
}

/// Releases each actor's file descriptor, as for [`BlockedUnboundRoots`].
impl Drop for ReapWorld {
    fn drop(&mut self) {
        for root in &self.roots {
            self.executor.remove_actor(root);
        }
    }
}

/// Over randomized roots (bound or not, quiesced or not, inside or past
/// their TTL), standing blockers switching on and off, pending paths
/// arriving between sweeps, and presence probes reporting directories
/// missing, the reaper makes the same decisions with whole-set restores as
/// with the path-by-path re-insertion they replaced.
#[test]
fn reap_decisions_match_path_by_path_restore_over_random_sequences() {
    const ROOTS: usize = 6;
    const SWEEPS: u64 = 10;
    let mut evictions = 0;
    let mut forgotten = 0;
    let mut standing_retained = 0;
    for seed in 1..=6 {
        let mut rng = Rng::new(seed);
        let base = Instant::now();
        let now_at = |sweep: u64| base + IDLE_ROOT_TTL + Duration::from_secs(1 + sweep);
        let mut legacy = ReapWorld::new(ROOTS, true);
        let mut current = ReapWorld::new(ROOTS, false);
        for index in 0..ROOTS {
            // A root last touched just before the first sweep stays inside
            // its TTL for the whole run; the others are past it from the start.
            let past_ttl = rng.chance(70);
            let last_touched = if past_ttl {
                base
            } else {
                now_at(0) - Duration::from_secs(1)
            };
            let quiesced = rng.chance(85);
            let bound = rng.chance(20);
            let standing = rng.chance(40);
            for world in [&mut legacy, &mut current] {
                world.set_meta(index, last_touched, quiesced);
                world.set_bound(index, bound);
                world.contexts[index].set_standing_artifact_exempt(standing);
            }
        }

        for sweep in 1..=SWEEPS {
            let mut absent = Vec::new();
            for index in 0..ROOTS {
                let root = &current.roots[index];
                let Some(meta) = current.live_roots.get(root) else {
                    continue;
                };
                // An evicted root's background release may still be running;
                // leave it out of deletion so no try-lock races it.
                let evicted = meta.idle_artifacts_evicted;
                if rng.chance(25) {
                    let standing = rng.chance(50);
                    for world in [&legacy, &current] {
                        world.contexts[index].set_standing_artifact_exempt(standing);
                    }
                }
                if rng.chance(15) {
                    let bound = rng.chance(50);
                    for world in [&mut legacy, &mut current] {
                        world.set_bound(index, bound);
                    }
                }
                if rng.chance(35) {
                    let set = rng.below(4);
                    let relative = (0..1 + rng.below(24))
                        .map(|_| format!("src/dir_{}/file_{}.rs", rng.below(5), rng.below(40)))
                        .collect::<Vec<_>>();
                    let corpus = rng.chance(20);
                    for world in [&legacy, &current] {
                        world.add_pending(index, set, &relative, corpus);
                    }
                }
                if !evicted && rng.chance(20) {
                    absent.push(index);
                }
            }
            let legacy_record = legacy.sweep(now_at(sweep), sweep, &absent);
            let current_record = current.sweep(now_at(sweep), sweep, &absent);
            assert_eq!(
                current_record, legacy_record,
                "seed {seed} sweep {sweep}: whole-set restore changed a reap decision"
            );
            evictions += current_record.evicted;
            forgotten += current_record.forgotten.len();
            standing_retained += current_record.census["unbound_blockers"]["standing_root"]
                .as_u64()
                .unwrap();
        }
    }
    // The sequences reached eviction, deleted-root retirement, and the
    // blocked take-and-restore round trip, so the comparison covered them.
    assert!(evictions > 0, "no sweep evicted a root");
    assert!(forgotten > 0, "no sweep retired a deleted root");
    assert!(standing_retained > 0, "no sweep restored a blocked root");
}
