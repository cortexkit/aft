//! Checkout semantic views through the runtime that serves views-on search:
//! shared planes across checkouts, embed counts across sessions, and edits
//! reaching the fill through both the watcher and AFT's own writes.

use super::*;
use crate::semantic_index::{
    EmbedTextCaps, LocalEmbeddingProvider, SemanticEmbeddingModel, SemanticIndex, SemanticResult,
};
use std::sync::atomic::AtomicUsize;

/// Store rows a filled one-chunk file leaves: its file run, its chunk-vector
/// row, and the marker row saying the run's vector matches that chunk row.
const ROWS_PER_ONE_CHUNK_FILE: u64 = 3;

#[test]
fn reconciled_delta_does_not_request_another_source_walk() {
    let manifest =
        super::super::manifest_v2::ManifestV2::new(super::super::manifest_v2::ManifestHeader {
            producers: Producers {
                trigram: "unused".into(),
                semantic: None,
                callgraph: "unused".into(),
            },
            head_tree: None,
            ignore_fingerprint: None,
            segment: None,
        });
    let mut delta = LiveDelta::new(Arc::new(ComposedGeneration::new(
        "published",
        manifest,
        None,
    )));
    let path = super::super::RelPath::new(b"file.rs".to_vec()).unwrap();
    let mut entry = super::super::snapshot::LiveEntry::new(DiskState::of_bytes(b"new"), 1);
    delta.apply(path.clone(), entry.clone());
    assert!(super::needs_reload(&delta.snapshot()));
    entry.reconciliation = super::super::readiness::ReconciledDelta::Reconciled;
    delta.apply(path.clone(), entry);
    assert!(!super::needs_reload(&delta.snapshot()));
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("file.rs")).unwrap();
    assert!(!super::needs_cold_reconcile(&delta.snapshot(), root.path()));
    delta.record_intent(path);
    assert!(super::needs_reload(&delta.snapshot()));
    assert!(super::needs_cold_reconcile(&delta.snapshot(), root.path()));
}

/// The test prints public setup, query answers, a v3 file inventory and exact
/// reader-floor bytes. Query expectations come from an independent cold index,
/// not the view.
#[test]
fn composition_no_marker_boundary_record() {
    use crate::migration::prune_legacy::{
        prune_legacy, CensusFinding, ProcessCensus, PruneExit, PruneOptions,
    };
    use crate::root_cache::StoragePaths;
    struct NoProcesses;
    impl ProcessCensus for NoProcesses {
        fn take(&self) -> Result<Vec<CensusFinding>, String> {
            Ok(Vec::new())
        }
    }
    // Inventory names and sizes without opening another descriptor on a live
    // SQLite file set. The reader floor is a separately encoded JSON file.
    fn inventory(root: &Path, path: &Path, files: &mut std::collections::BTreeMap<String, u64>) {
        if !path.exists() {
            return;
        }
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                inventory(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    std::fs::metadata(&path).unwrap().len(),
                );
            }
        }
    }
    let mut answers = Vec::new();
    for views in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let temp = std::fs::canonicalize(temp.path()).unwrap();
        let storage = temp.join("storage");
        let root = temp.join("project");
        write_tree(&root, FILES);
        std::fs::create_dir_all(&storage).unwrap();
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                ..Default::default()
            },
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "composition-boundary", "command": "configure", "project_root": root, "storage_dir": storage,
            "harness": "runner", "session_id": "alfonso:sidekick-00000000-0000-4001-98dd-383a2dbeca78",
            "config": [{ "tier": "user", "source": "/u/aft.jsonc", "doc": serde_json::json!({
                "views": { "enabled": views }, "indexes": { "trigram": false, "semantic": true, "callgraph": false },
                "semantic": { "backend": "openai_compatible", "model": "boundary-no-network", "base_url": "http://127.0.0.1:1", "timeout_ms": 50 }
            }).to_string() }]
        })).unwrap();
        let response = crate::commands::configure::handle_configure(&request, &ctx);
        assert!(response.success, "{}", response.data);
        crate::commands::configure::drain_deferred_configure_maintenance(&ctx);
        assert!(
            ctx.semantic_refresh_sender().is_none(),
            "the sidekick fixture must not start an embedding worker"
        );
        let answer = if views {
            let family = crate::search_index::artifact_cache_key(&root);
            let scope = crate::path_identity::project_scope_key(&root);
            let runtime = CheckoutSemantic::new(
                &StoragePaths::same(&storage),
                &family,
                &scope,
                &root,
                producer(),
                Weak::new(),
            )
            .unwrap();
            runtime.load().unwrap();
            refresh(&runtime, &Model::default());
            let result = query(&runtime, "alpha cache");
            assert!(result.complete(), "{result:?}");
            let rows = rows(&root, &result.results);
            let registry = super::super::registry::FamilyRegistry::open(&storage, &family).unwrap();
            super::super::eviction::enforce(&registry, Default::default()).unwrap();
            rows
        } else {
            cold(&root, "alpha cache").0
        };
        assert!(!answer.is_empty());
        for _ in 0..2 {
            let report = crate::storage_retention::run_pass_with_budget_for_test(
                &StoragePaths::same(&storage),
                Duration::from_secs(120),
            );
            assert!(report.timings_ms.contains_key("blobs_v3"), "{report:?}");
        }
        let floor_before = std::fs::read(storage.join("reader-floor.json")).unwrap();
        let mut before = Default::default();
        inventory(&storage, &storage.join("blobs/v3"), &mut before);
        let mut out = Vec::new();
        let exit = prune_legacy(
            &storage,
            &PruneOptions {
                yes: false,
                now: std::time::SystemTime::now(),
            },
            &NoProcesses,
            None,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, PruneExit::Clean);
        assert!(String::from_utf8(out).unwrap().contains("Dry run"));
        let mut v3 = Default::default();
        inventory(&storage, &storage.join("blobs/v3"), &mut v3);
        assert_eq!(before, v3, "dry-run must not mutate the v3 namespace");
        assert_eq!(
            std::fs::read(storage.join("reader-floor.json")).unwrap(),
            floor_before
        );
        assert!(!storage.join("views-harness.marker").exists());
        assert!(
            !storage.join("blobs/v3").exists(),
            "an unrouted entrypoint wrote v3 without the marker"
        );
        let floor = std::fs::read_to_string(storage.join("reader-floor.json")).unwrap();
        assert!(!floor.contains("views_v3"));
        eprintln!(
            "COMPOSITION_BOUNDARY_RECORD {}",
            serde_json::json!({ "views": views, "answers": answer, "v3_files": v3, "floor": floor })
        );
        answers.push(answer);
    }
    assert_eq!(answers[0], answers[1]);
}

#[test]
fn composition_prune_dry_run_preserves_populated_v3_inventory() {
    use crate::migration::prune_legacy::{
        prune_legacy, CensusFinding, ProcessCensus, PruneExit, PruneOptions,
    };
    struct NoProcesses;
    impl ProcessCensus for NoProcesses {
        fn take(&self) -> Result<Vec<CensusFinding>, String> {
            Ok(Vec::new())
        }
    }
    fn inventory(root: &Path, path: &Path, out: &mut std::collections::BTreeMap<String, [u8; 32]>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                inventory(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    *blake3::hash(&std::fs::read(&path).unwrap()).as_bytes(),
                );
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path();
    let v3 = storage.join("blobs/v3/populated");
    std::fs::create_dir_all(v3.join("views/worker")).unwrap();
    // A legacy pruner must preserve opaque newer files, not just rows it can decode.
    // These fixture files are opaque bytes with no live SQLite connection.
    std::fs::write(v3.join("semantic.sqlite"), b"opaque future plane payload").unwrap();
    std::fs::write(
        v3.join("views/worker/manifest-future.json"),
        br#"{"format_version":3,"entries":["kept.rs"]}"#,
    )
    .unwrap();
    crate::reader_floor::raise(
        storage,
        &[
            (crate::persisted_format::PersistedStore::ViewsV3Registry, 1),
            (crate::persisted_format::PersistedStore::ViewsV3Store, 1),
        ],
    )
    .unwrap();
    let mut before = Default::default();
    inventory(storage, &v3, &mut before);
    assert_eq!(before.len(), 2);
    let floor = std::fs::read(storage.join("reader-floor.json")).unwrap();
    let mut output = Vec::new();
    assert_eq!(
        prune_legacy(
            storage,
            &PruneOptions {
                yes: false,
                now: std::time::SystemTime::now()
            },
            &NoProcesses,
            None,
            &mut output
        )
        .unwrap(),
        PruneExit::Clean
    );
    let mut after = Default::default();
    if v3.exists() {
        inventory(storage, &v3, &mut after);
    }
    assert_eq!(before, after, "dry-run changed populated v3 files");
    assert_eq!(
        floor,
        std::fs::read(storage.join("reader-floor.json")).unwrap()
    );
}

#[test]
fn lane_paused_view_fill_waits_without_rechunking_and_resumes() {
    use crate::semantic_index::{lane_pause_chunk_calls, with_lane_pause_clock, PauseRetryScope};
    use crate::synapse_embed::pause_test_support::*;
    with_lane_pause_clock(START, |clock| {
        with_fake_synapse(clock.clone(), None, |config, requests| {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("checkout");
            let storage = directory.path().join("storage");
            write_tree(
                &root,
                &[("pause.ts", "export function keyword() { return 3; }\n")],
            );
            let runtime = open(&storage, "paused", &root);
            let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
            let _policy =
                PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true);
            let before = lane_pause_chunk_calls();
            let mut attempts = Vec::new();
            let slot = Arc::new(CheckoutSemanticSlot::default());
            let (epoch, _wake) = slot.begin();
            let outcome = catch_up(
                &Arc::downgrade(&slot),
                epoch,
                &runtime,
                &fast_schedule(&root),
                FillBudget::default(),
                &mut |texts| {
                    attempts.push((
                        clock.load(Ordering::SeqCst),
                        lane_pause_chunk_calls() - before,
                    ));
                    model.embed(texts)
                },
            );
            assert!(matches!(outcome, FillOutcome::Settled));
            assert_eq!(attempts, vec![(START, 1), (START + PAUSE_MS, 1)]);
            assert_eq!(lane_pause_chunk_calls() - before, 1);
            assert_eq!(requests.lock().unwrap().len(), 3);
            assert!(!runtime
                .search(&[1.0, 0.0], 10, &|_| true)
                .unwrap()
                .results
                .is_empty());
        });
    });
}

#[test]
fn shape_compiling_view_fill_waits_out_the_compile_and_resumes() {
    use crate::semantic_index::{lane_pause_chunk_calls, with_lane_pause_clock, PauseRetryScope};
    use crate::synapse_embed::pause_test_support::*;
    with_lane_pause_clock(START, |clock| {
        with_compiling_synapse(clock.clone(), 90_000, |config, requests| {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("checkout");
            let storage = directory.path().join("storage");
            write_tree(
                &root,
                &[("compile.ts", "export function keyword() { return 3; }\n")],
            );
            let runtime = open(&storage, "compiling", &root);
            let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
            let _policy =
                PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true);
            let before = lane_pause_chunk_calls();
            let mut attempts = Vec::new();
            let slot = Arc::new(CheckoutSemanticSlot::default());
            let (epoch, _wake) = slot.begin();
            let outcome = catch_up(
                &Arc::downgrade(&slot),
                epoch,
                &runtime,
                &fast_schedule(&root),
                FillBudget::default(),
                &mut |texts| {
                    attempts.push((
                        clock.load(Ordering::SeqCst),
                        lane_pause_chunk_calls() - before,
                    ));
                    model.embed(texts)
                },
            );
            assert!(matches!(outcome, FillOutcome::Settled));
            // The 90 s hint outlasts the fill's 5 s backoff, so it sets the wait.
            assert_eq!(attempts, vec![(START, 1), (START + 90_000, 1)]);
            assert_eq!(lane_pause_chunk_calls() - before, 1);
            assert_eq!(requests.lock().unwrap().len(), 3);
            assert!(!runtime
                .search(&[1.0, 0.0], 10, &|_| true)
                .unwrap()
                .results
                .is_empty());
        });
    });
}

#[test]
fn lane_paused_view_fingerprint_waits_before_checkout_preparation() {
    use crate::semantic_index::{lane_pause_chunk_calls, with_lane_pause_clock, PauseRetryScope};
    use crate::synapse_embed::pause_test_support::*;
    with_lane_pause_clock(START, |clock| {
        with_fake_synapse(clock.clone(), None, |semantic, requests| {
            let root = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            write_tree(root.path(), FILES);
            let slot = Arc::new(CheckoutSemanticSlot::default());
            let (epoch, wake) = slot.begin();
            let config = WorkerConfig {
                root: root.path().to_path_buf(),
                storage: crate::root_cache::StoragePaths::same(storage.path()),
                family: "family".into(),
                scope: "paused-fingerprint".into(),
                semantic: semantic.clone(),
                status: Arc::new(RwLock::new(crate::context::SemanticIndexStatus::Disabled)),
                drivers: InstalledDriver::default(),
                schedule: fast_schedule(root.path()),
            };
            let mut model = SemanticEmbeddingModel::from_config(&semantic).unwrap();
            let before = lane_pause_chunk_calls();
            let _policy = PauseRetryScope::new(
                config.schedule.retry_initial,
                config.schedule.retry_max,
                || true,
            );
            let fingerprint = retry_activation(
                &Arc::downgrade(&slot),
                epoch,
                &wake,
                &config,
                "embedding model",
                || model.fingerprint_for_view(&semantic),
            )
            .unwrap();
            assert_eq!(fingerprint.dimension, 2);
            assert_eq!(lane_pause_chunk_calls(), before);
            assert!(config.drivers.get().is_none());
            let times = requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, request)| request["method"] == "embed.query")
                .map(|(time, _)| *time)
                .collect::<Vec<_>>();
            assert_eq!(times, vec![START, START + PAUSE_MS]);
        });
    });
}

#[test]
fn lane_paused_view_wait_releases_admission_refresh_and_family_claims() {
    use crate::semantic_index::{
        with_lane_pause_clock, with_lane_pause_wait_hook, PauseRetryScope,
    };
    use crate::synapse_embed::pause_test_support::*;
    with_lane_pause_clock(START, |clock| {
        with_fake_synapse(clock.clone(), None, |config, _requests| {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("checkout");
            let storage = directory.path().join("storage");
            write_tree(
                &root,
                &[("pause.ts", "export function released() { return 3; }\n")],
            );
            let runtime = Arc::new(open(&storage, "paused-resources", &root));
            let arena = super::super::semantic_arena::arena_for(&storage, "family");
            let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
            let slot = Arc::new(CheckoutSemanticSlot::default());
            let (epoch, _wake) = slot.begin();
            let mut schedule = fast_schedule(&root);
            schedule.limiter = crate::cold_build_limiter::isolated_limiter(1);
            let preparation = crate::cold_build_limiter::isolated_limiter(1);
            let cold_probe = schedule.limiter.clone();
            let prep_probe = preparation.clone();
            let runtime_probe = runtime.clone();
            let clock_probe = clock.clone();
            let waited = Arc::new(AtomicUsize::new(0));
            let observed = waited.clone();
            let _policy =
                PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true);
            let outcome = with_lane_pause_wait_hook(
                move |delay| {
                    let _cold = cold_probe
                        .try_acquire()
                        .expect("pause retained cold admission");
                    let _preparation = prep_probe
                        .try_acquire()
                        .expect("pause retained preparation admission");
                    let _refresh = runtime_probe
                        .refresh
                        .try_lock()
                        .expect("pause retained refresh mutex");
                    assert!(
                        arena.pause_resources_are_free(),
                        "pause retained a family gate or file claim"
                    );
                    observed.fetch_add(1, Ordering::SeqCst);
                    clock_probe.fetch_add(delay.as_millis() as u64, Ordering::SeqCst);
                },
                || {
                    catch_up_with_preparation_limiter(
                        &Arc::downgrade(&slot),
                        epoch,
                        &runtime,
                        &schedule,
                        FillBudget::default(),
                        &mut |texts| model.embed(texts),
                        &preparation,
                    )
                },
            );
            assert!(matches!(outcome, FillOutcome::Settled));
            assert_eq!(waited.load(Ordering::SeqCst), 1);
        });
    });
}

#[test]
fn normal_view_fill_has_zero_checkpoint_work() {
    use crate::semantic_index::{with_checkpoint_work, PauseRetryScope};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("checkout");
    write_tree(&root, FILES);
    let runtime = open(
        &directory.path().join("storage"),
        "normal-no-checkpoints",
        &root,
    );
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, _wake) = slot.begin();
    let model = Model::default();
    let (outcome, work) = with_checkpoint_work(|| {
        let _policy =
            PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true);
        catch_up(
            &Arc::downgrade(&slot),
            epoch,
            &runtime,
            &fast_schedule(&root),
            FillBudget::default(),
            &mut |texts| model.embed(texts),
        )
    });
    assert!(matches!(outcome, FillOutcome::Settled));
    assert!(model.calls() > 0);
    assert_eq!(
        work,
        Default::default(),
        "healthy view fill copied pause checkpoints"
    );
}

#[test]
fn lane_paused_view_cursor_retains_completed_and_unattempted_chunks() {
    use crate::semantic_index::{lane_pause_chunk_calls, with_lane_pause_clock, PauseRetryScope};
    use crate::synapse_embed::{pause_test_support::*, LanePause, SynapseEmbeddingError};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("checkout");
    write_tree(
        &root,
        &[
            ("a.rs", "/// First.\npub fn first(x: u64) -> u64 { x + 1 }\n/// Second.\npub fn second(x: u64) -> u64 { x + 2 }\n"),
            ("b.rs", "/// Third.\npub fn third(x: u64) -> u64 { x + 3 }\n/// Fourth.\npub fn fourth(x: u64) -> u64 { x + 4 }\n"),
            ("c.rs", "/// Fifth.\npub fn fifth(x: u64) -> u64 { x + 5 }\n/// Sixth.\npub fn sixth(x: u64) -> u64 { x + 6 }\n"),
        ],
    );
    let runtime = open(
        &directory.path().join("storage"),
        "paused-owned-cursor",
        &root,
    );
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, _wake) = slot.begin();
    with_lane_pause_clock(START, |clock| {
        let _policy =
            PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true);
        let before = lane_pause_chunk_calls();
        let mut inputs = Vec::new();
        let outcome = catch_up(
            &Arc::downgrade(&slot),
            epoch,
            &runtime,
            &fast_schedule(&root),
            FillBudget {
                max_batch: 1,
                ..FillBudget::default()
            },
            &mut |texts: Vec<String>| {
                inputs.extend(texts.clone());
                if inputs.len() == 2 && clock.load(Ordering::SeqCst) == START {
                    Err(SynapseEmbeddingError::LanePaused(LanePause {
                        lane_id: MODEL.into(),
                        fingerprint: Some("pinned".into()),
                        retry_after_ms: Some(PAUSE_MS),
                        paused_until_ms: START + PAUSE_MS,
                        reason: "maintenance".into(),
                        cause: Default::default(),
                        safe_to_retry_same_request: true,
                        record_id: 0,
                    })
                    .to_string())
                } else {
                    Ok(vec![vec![1.0, 0.0]; texts.len()])
                }
            },
        );
        assert!(matches!(outcome, FillOutcome::Settled));
        assert_eq!(
            runtime
                .search(&[1.0, 0.0], 100, &|_| true)
                .unwrap()
                .results
                .len(),
            9
        );
        assert_eq!(inputs.len(), 10, "a completed vector was embedded twice");
        assert_eq!(inputs[1], inputs[2]);
        assert_eq!(
            lane_pause_chunk_calls() - before,
            3,
            "unattempted prepared files were re-chunked"
        );
    });
}

/// The error a Synapse client returns when the embedding lane refuses a
/// request with `lane_paused`, as the client renders it.
fn lane_pause_error() -> String {
    use crate::synapse_embed::{pause_test_support::*, LanePause, SynapseEmbeddingError};
    SynapseEmbeddingError::LanePaused(LanePause {
        lane_id: MODEL.into(),
        fingerprint: Some("pinned".into()),
        retry_after_ms: Some(PAUSE_MS),
        paused_until_ms: START + PAUSE_MS,
        reason: "maintenance".into(),
        cause: Default::default(),
        safe_to_retry_same_request: true,
        record_id: 0,
    })
    .to_string()
}

/// The root loses its subc route (unbind) while the Synapse embedding lane is
/// paused, then the route comes back. The fill parks instead of stopping, and
/// the wake a rebind sends lets it finish indexing.
#[test]
fn lane_paused_fill_parks_on_disconnect_and_resumes_on_rebind() {
    use crate::semantic_index::{with_lane_pause_wait_hook, PauseRetryScope};
    use std::sync::atomic::AtomicBool;
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, wake, runtime) = served_lane(storage.path(), root.path());
    let observer = FillObserver::new(root.path().to_path_buf());
    let bound = Arc::new(AtomicBool::new(true));
    let model = Arc::new(Model::default());
    let worker = {
        let weak = Arc::downgrade(&slot);
        let bound = Arc::clone(&bound);
        let model = Arc::clone(&model);
        let root = root.path().to_path_buf();
        std::thread::spawn(move || {
            let strict = Arc::clone(&bound);
            let _policy =
                PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true)
                    .with_pause_current(move || strict.load(Ordering::SeqCst));
            let mut paused = false;
            // A pause wait that is not cut short returns at once instead of
            // sleeping out the announced pause.
            with_lane_pause_wait_hook(
                |_| {},
                || {
                    serve_fills(&weak, epoch, &wake, &fast_schedule(&root), &mut |texts| {
                        if !paused {
                            paused = true;
                            bound.store(false, Ordering::SeqCst);
                            return Err(lane_pause_error());
                        }
                        model.embed(texts)
                    });
                },
            );
        })
    };
    let hang_guard = Duration::from_secs(20);
    let parked = observer.rx.recv_timeout(hang_guard).ok();
    if parked != Some("parked") {
        slot.clear();
        worker.join().unwrap();
        panic!("a disconnect during the pause stopped the fill: {parked:?}");
    }
    assert_eq!(model.calls(), 0);
    assert!(slot.is_current(epoch), "the parked lane gave up its slot");
    bound.store(true, Ordering::SeqCst);
    slot.wake();
    let deadline = Instant::now() + hang_guard;
    let finished = loop {
        match observer.rx.recv_deadline(deadline) {
            Ok("wake") => continue,
            event => break event.ok(),
        }
    };
    slot.clear();
    worker.join().unwrap();
    assert_eq!(finished, Some("settled"));
    assert!(model.calls() > 0);
    assert!(!runtime
        .search(&vector("alpha_total"), 100, &|_| true)
        .unwrap()
        .results
        .is_empty());
}

/// The root loses its subc route while the lane is still starting and its
/// embedding-model fingerprint probe was refused with `lane_paused`: the probe
/// parks, and runs again once a rebind wakes the lane.
#[test]
fn lane_paused_activation_parks_on_disconnect_and_resumes_on_rebind() {
    use crate::semantic_index::{with_lane_pause_wait_hook, PauseRetryScope};
    use std::sync::atomic::AtomicBool;
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let observer = FillObserver::new(root.path().to_path_buf());
    let bound = Arc::new(AtomicBool::new(true));
    let worker = {
        let weak = Arc::downgrade(&slot);
        let bound = Arc::clone(&bound);
        let root = root.path().to_path_buf();
        let storage = storage.path().to_path_buf();
        std::thread::spawn(move || {
            let config = WorkerConfig {
                root: root.clone(),
                storage: crate::root_cache::StoragePaths::same(&storage),
                family: "family".into(),
                scope: "paused-activation".into(),
                semantic: crate::config::SemanticBackendConfig::default(),
                status: Arc::new(RwLock::new(crate::context::SemanticIndexStatus::Disabled)),
                drivers: InstalledDriver::default(),
                schedule: fast_schedule(&root),
            };
            let strict = Arc::clone(&bound);
            let _policy =
                PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true)
                    .with_pause_current(move || strict.load(Ordering::SeqCst));
            let mut attempts = 0;
            let result = with_lane_pause_wait_hook(
                |_| {},
                || {
                    retry_activation(&weak, epoch, &wake, &config, "embedding model", || {
                        attempts += 1;
                        if attempts == 1 {
                            bound.store(false, Ordering::SeqCst);
                            return Err(lane_pause_error());
                        }
                        Ok(attempts)
                    })
                },
            );
            (result, attempts)
        })
    };
    let parked = observer.rx.recv_timeout(Duration::from_secs(20)).ok();
    if parked != Some("parked") {
        slot.clear();
        let (result, _) = worker.join().unwrap();
        panic!("a disconnect during the pause stopped activation: {parked:?} {result:?}");
    }
    bound.store(true, Ordering::SeqCst);
    slot.wake();
    let (result, attempts) = worker.join().unwrap();
    assert_eq!(result, Ok(2));
    assert_eq!(attempts, 2);
}

#[test]
fn released_lane_frees_only_its_own_epoch() {
    let slot = CheckoutSemanticSlot::default();
    let (replaced, _replaced_wake) = slot.begin();
    let (current, wake) = slot.begin();
    assert!(
        !slot.release(replaced),
        "a replaced worker released its successor"
    );
    assert!(slot.active() && slot.is_current(current));
    assert!(slot.release(current));
    // `active()` now reports no lane, which is what lets the root's next
    // artifact check start a new worker.
    assert!(!slot.active());
    assert!(!slot.is_current(current));
    assert!(
        wake.recv().is_err(),
        "the released worker's channel stayed open"
    );
}

/// Waits up to ten seconds for `done`, failing with `what`.
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Idle eviction of a root lets go of its loaded semantic lane. The view
/// unbinds and the family arena drops the run only that view held, while the
/// runs another live view of the family still reads stay resident.
#[test]
fn idle_eviction_releases_the_lane_and_trims_only_its_own_runs() {
    let storage = tempfile::tempdir().unwrap();
    let evicted_root = tempfile::tempdir().unwrap();
    let live_root = tempfile::tempdir().unwrap();
    write_tree(evicted_root.path(), FILES);
    write_tree(
        evicted_root.path(),
        &[(
            "src/delta.rs",
            "pub fn delta_only_here() -> u8 {\n    4\n}\n",
        )],
    );
    write_tree(live_root.path(), FILES);
    let model = Model::default();
    let live = open(storage.path(), "idle-live", live_root.path());
    refresh(&live, &model);

    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config::default(),
    );
    let slot = ctx.checkout_semantic();
    let (epoch, wake) = slot.begin();
    let runtime = Arc::new(
        CheckoutSemantic::new(
            &crate::root_cache::StoragePaths::same(storage.path()),
            "family",
            "idle-evicted",
            evicted_root.path(),
            producer(),
            Arc::downgrade(slot),
        )
        .unwrap(),
    );
    ctx.installed_checkout_driver()
        .install(Arc::clone(runtime.driver()));
    runtime.load().unwrap();
    refresh(&runtime, &model);
    assert!(slot.install(epoch, Arc::clone(&runtime)));
    assert!(Arc::ptr_eq(runtime.plane(), live.plane()));

    // The root's census row counts the view's own overlay, not the arena.
    let semantic = ctx.memory_root_snapshot().semantic;
    let overlay = runtime.memory().private;
    assert_eq!(
        semantic.counts.get("checkout_overlay_bytes"),
        Some(&overlay)
    );
    assert_eq!(semantic.estimated_bytes, Some(overlay));

    let arena = runtime.plane().arena("family");
    let before = arena.memory();
    assert_eq!(
        before.runs, 4,
        "three shared runs and one of the evicted view's own"
    );
    let released = Arc::downgrade(&runtime);
    drop(runtime);

    assert!(ctx.try_evict_idle_artifacts());
    assert!(
        !slot.active(),
        "idle eviction kept the semantic lane loaded"
    );
    assert!(
        wake.recv().is_err(),
        "the released lane's worker channel stayed open"
    );
    wait_until("the released semantic view was never dropped", || {
        released.upgrade().is_none()
    });
    wait_until("the released view's driver stayed installed", || {
        ctx.installed_checkout_driver().get().is_none()
    });
    // The view unbinds inside the runtime's drop, after its last strong
    // reference is gone, so wait for the trim rather than read it at once.
    wait_until("the arena never trimmed the released view's run", || {
        arena.memory().runs < before.runs
    });
    let after = arena.memory();
    assert_eq!(after.runs, 3, "only the evicted view's own run is dropped");
    assert!(after.bytes < before.bytes);
    assert!(
        query(&live, "alpha_total").complete(),
        "the live view lost a shared run"
    );
}

/// A lane that never finished loading (its worker waiting for a start signal
/// or retrying an unreachable backend) is released by idle eviction too, so
/// its worker exits instead of waiting for as long as the root stays unbound.
#[test]
fn idle_eviction_releases_a_lane_that_is_still_loading() {
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config::default(),
    );
    let slot = ctx.checkout_semantic();
    let (epoch, wake) = slot.begin();
    assert!(slot.active(), "a begun lane reads as loading");

    assert!(ctx.try_evict_idle_artifacts());
    assert!(!slot.active(), "idle eviction kept a loading semantic lane");
    assert!(
        !slot.is_current(epoch),
        "the loading worker's epoch must end"
    );
    assert!(
        wake.recv().is_err(),
        "the loading lane's worker channel stayed open"
    );
}

/// The memory census counts a family's arena runs once, however many views
/// of the family are loaded, and adds them once to the process total.
#[test]
fn census_counts_a_family_arena_once_across_its_views() {
    let storage = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_tree(first.path(), FILES);
    write_tree(second.path(), FILES);
    let model = Model::default();
    let a = open(storage.path(), "census-a", first.path());
    let b = open(storage.path(), "census-b", second.path());
    refresh(&a, &model);
    refresh(&b, &model);
    let arena = a.plane().arena("family");
    let memory = arena.memory();
    assert_eq!(memory.runs, 3, "the two views share every run");
    assert!(memory.bytes > 0);

    let snapshot = crate::memory::MemorySnapshot::new("ready", Default::default());
    let storage_label = arena.storage().display().to_string();
    let rows: Vec<_> = snapshot
        .semantic_arenas
        .families
        .iter()
        .filter(|row| row.storage == storage_label)
        .collect();
    assert_eq!(rows.len(), 1, "the family was counted per view: {rows:?}");
    assert_eq!(
        (
            rows[0].family.as_str(),
            rows[0].runs,
            rows[0].chunks,
            rows[0].bytes
        ),
        ("family", memory.runs, memory.chunks, memory.bytes)
    );
    let families = &snapshot.semantic_arenas.families;
    let process = &snapshot.process;
    assert_eq!(
        process.semantic_arena_bytes,
        families.iter().map(|row| row.bytes).sum::<u64>()
    );
    assert_eq!(
        process.semantic_resident_chunks,
        families.iter().map(|row| row.chunks as u64).sum::<u64>()
    );
    assert_eq!(
        process.total_attributed_bytes,
        snapshot.shared_semantic_bases.estimated_bytes.unwrap_or(0)
            + process.semantic_arena_bytes
            + process.sqlite.memory_used_bytes,
        "the arena is not in the process total exactly once"
    );
}

const FILES: &[(&str, &str)] = &[
    (
        "src/alpha.rs",
        "pub fn alpha_total(values: &[u32]) -> u32 {\n    values.iter().sum()\n}\n\npub fn alpha_label() -> &'static str {\n    \"alpha\"\n}\n",
    ),
    (
        "src/beta.rs",
        "pub struct BetaCache {\n    entries: Vec<String>,\n}\n\nimpl BetaCache {\n    pub fn insert(&mut self, value: String) {\n        self.entries.push(value);\n    }\n}\n",
    ),
    (
        "src/gamma.rs",
        "fn gamma_parse(input: &str) -> Option<u64> {\n    input.trim().parse().ok()\n}\n",
    ),
    ("notes.txt", "not a semantic source\n"),
];

/// A deterministic model: equal texts get equal vectors.
fn vector(text: &str) -> Vec<f32> {
    blake3::hash(text.as_bytes()).as_bytes()[..16]
        .iter()
        .map(|byte| (f32::from(*byte) - 127.5) / 127.5)
        .collect()
}

#[derive(Default)]
struct Model {
    texts: AtomicUsize,
    calls: AtomicUsize,
}

impl Model {
    fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        self.texts.fetch_add(texts.len(), Ordering::SeqCst);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(texts.iter().map(|text| vector(text)).collect())
    }

    fn texts(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn write_tree(root: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

fn producer() -> SemanticProducer {
    SemanticProducer::current("runtime-test-model", EmbedTextCaps::default())
}

fn open(storage: &Path, scope: &str, root: &Path) -> CheckoutSemantic {
    let runtime = CheckoutSemantic::new(
        &crate::root_cache::StoragePaths::same(storage),
        "family",
        scope,
        root,
        producer(),
        Weak::new(),
    )
    .unwrap();
    runtime.driver().register_write_intent();
    runtime.load().unwrap();
    runtime
}

fn refresh(runtime: &CheckoutSemantic, model: &Model) -> FillReport {
    runtime
        .refresh(FillBudget::default(), &mut |texts| model.embed(texts))
        .unwrap()
}

type Row = (String, String, u32, u32);

fn rows(root: &Path, results: &[SemanticResult]) -> Vec<Row> {
    results
        .iter()
        .map(|result| {
            (
                result
                    .file
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                result.name.clone(),
                result.start_line,
                result.score.to_bits(),
            )
        })
        .collect()
}

/// A full legacy build of `root`: its ranked rows for `query` and the number
/// of texts it embedded, which is the number of unique chunks.
fn cold(root: &Path, query: &str) -> (Vec<Row>, usize) {
    let files = ConfiguredMembershipWalker.files(root).unwrap();
    let mut texts = 0;
    let index = SemanticIndex::build(
        root,
        &files,
        &mut |batch: Vec<String>| {
            texts += batch.len();
            Ok(batch.iter().map(|text| vector(text)).collect())
        },
        64,
    )
    .unwrap();
    (rows(root, &index.search(&vector(query), 100)), texts)
}

fn query(runtime: &CheckoutSemantic, text: &str) -> SemanticQuery {
    runtime.search(&vector(text), 100, &|_| true).unwrap()
}

#[test]
fn base_ref_selection_child() {
    let Ok(mode) = std::env::var("AFT_BASE_REF_SELECTION_CHILD") else {
        return;
    };
    let _ = env_logger::Builder::new()
        .filter_level(log::LevelFilter::Warn)
        .try_init();
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    if mode == "marked" || mode == "default" {
        std::fs::write(storage.path().join("views-harness.marker"), b"test\n").unwrap();
    }
    let first = CheckoutSemantic::new(
        &crate::root_cache::StoragePaths::same(storage.path()),
        "family",
        "first",
        root.path(),
        producer(),
        Weak::new(),
    )
    .unwrap();
    let second = CheckoutSemantic::new(
        &crate::root_cache::StoragePaths::same(storage.path()),
        "family",
        "second",
        root.path(),
        producer(),
        Weak::new(),
    )
    .unwrap();
    assert_eq!(first.base_ref_prototype, mode == "marked");
    assert_eq!(second.base_ref_prototype, mode == "marked");
    assert_eq!(first.installed().generation().name(), "empty");
    assert_eq!(second.installed().generation().name(), "empty");
    let active = super::super::io::work_snapshot()["base_ref_prototype"]
        .as_bool()
        .unwrap_or(false);
    assert_eq!(
        active,
        mode == "marked",
        "prototype selection must be reported"
    );
    first.load().unwrap();
    refresh(&first, &Model::default());
    second.load().unwrap();
    assert_eq!(
        second
            .installed()
            .generation()
            .name()
            .starts_with("base-ref-"),
        mode == "marked" && cfg!(unix)
    );
    if mode != "marked" {
        assert!(lock(&first.base_ref_pin).is_none());
        assert!(lock(&second.base_ref_pin).is_none());
    }
}

fn base_ref_selection(mode: &str, enabled: bool) -> std::process::Output {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "views::semantic_runtime::tests::base_ref_selection_child",
            "--nocapture",
        ])
        .env("AFT_BASE_REF_SELECTION_CHILD", mode);
    if enabled {
        child.env("AFT_VIEWS_BASE_REF_PROTOTYPE", "1");
    } else {
        child.env_remove("AFT_VIEWS_BASE_REF_PROTOTYPE");
    }
    child
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = child.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("selection child exceeded 30 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    if output.status.success() {
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"),
            "selection child did not execute its exact test"
        );
    }
    output
}

#[test]
fn base_ref_marker_and_variable_are_required() {
    let output = base_ref_selection("marked", true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let output = base_ref_selection("default", false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn base_ref_unmarked_storage_warns_exactly_once() {
    let output = base_ref_selection("unmarked", true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let warnings = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        warnings.matches("AFT_VIEWS_BASE_REF_PROTOTYPE").count(),
        1,
        "{warnings}"
    );
}

#[cfg(unix)]
fn base_ref_runtime(storage: &Path, scope: &str, root: &Path) -> CheckoutSemantic {
    let mut runtime = CheckoutSemantic::new(
        &crate::root_cache::StoragePaths::same(storage),
        "family",
        scope,
        root,
        producer(),
        Weak::new(),
    )
    .unwrap();
    if !runtime.base_ref_prototype {
        runtime.base_ref_prototype = true;
        super::super::io::BASE_REF_PROTOTYPE_USERS.fetch_add(1, Ordering::Relaxed);
    }
    runtime.driver().register_write_intent();
    runtime.load().unwrap();
    runtime
}

#[cfg(unix)]
#[test]
fn base_ref_top_k_matches_strict_including_prebind_edit() {
    let storage = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    write_tree(main.path(), FILES);
    write_tree(worker.path(), FILES);
    let model = Model::default();
    let base = base_ref_runtime(storage.path(), "main", main.path());
    refresh(&base, &model);
    for modified in [false, true] {
        if modified {
            std::fs::write(
                worker.path().join("src/alpha.rs"),
                "pub fn distinct_worker_value() -> u64 { 9981 }\n",
            )
            .unwrap();
        }
        let suffix = if modified { "edited" } else { "clean" };
        let prototype = base_ref_runtime(
            storage.path(),
            &format!("prototype-{suffix}"),
            worker.path(),
        );
        assert_eq!(
            prototype
                .installed()
                .generation()
                .name()
                .starts_with("base-ref-"),
            !modified
        );
        if !modified {
            assert_eq!(prototype.driver().take_reconcile_work().walks, 0);
        }
        refresh(&prototype, &model);
        let strict = open(storage.path(), &format!("strict-{suffix}"), worker.path());
        refresh(&strict, &model);
        for text in [
            "alpha total",
            "beta cache insert",
            "gamma parse",
            "distinct worker value",
        ] {
            for top_k in [1, 3, 100] {
                let expected = strict.search(&vector(text), top_k, &|_| true).unwrap();
                let actual = prototype.search(&vector(text), top_k, &|_| true).unwrap();
                assert!(actual.complete());
                assert_eq!(
                    rows(worker.path(), &actual.results),
                    rows(worker.path(), &expected.results)
                );
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn base_ref_membership_changes_take_strict_path() {
    for change in ["add", "delete", "symlink"] {
        let storage = tempfile::tempdir().unwrap();
        let main = tempfile::tempdir().unwrap();
        let worker = tempfile::tempdir().unwrap();
        write_tree(main.path(), FILES);
        write_tree(worker.path(), FILES);
        let base = base_ref_runtime(storage.path(), "main", main.path());
        refresh(&base, &Model::default());
        match change {
            "add" => std::fs::write(worker.path().join("new.rs"), "fn added() {}\n").unwrap(),
            "delete" => std::fs::remove_file(worker.path().join("src/alpha.rs")).unwrap(),
            _ => {
                std::fs::remove_file(worker.path().join("src/alpha.rs")).unwrap();
                std::os::unix::fs::symlink(
                    main.path().join("src/alpha.rs"),
                    worker.path().join("src/alpha.rs"),
                )
                .unwrap();
            }
        }
        let runtime = base_ref_runtime(storage.path(), "worker", worker.path());
        assert!(
            !runtime
                .installed()
                .generation()
                .name()
                .starts_with("base-ref-"),
            "{change}"
        );
        assert!(runtime.driver().take_reconcile_work().walks > 0, "{change}");
    }
}

#[test]
fn base_ref_pending_is_not_ready() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let runtime = open(storage.path(), "main", root.path());
    assert!(!base_ref_ready(&runtime.installed()));
    refresh(&runtime, &Model::default());
    assert!(base_ref_ready(&runtime.installed()));
}

#[cfg(unix)]
#[test]
fn base_ref_does_not_inherit_private_git_administration() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let runtime = base_ref_runtime(storage.path(), "builder", root.path());
    refresh(&runtime, &Model::default());
    let mut manifest = runtime.installed().generation().flatten().unwrap().clone();
    let git = super::super::RelPath::from_os_path(Path::new(".git")).unwrap();
    let bytes = b"gitdir: /private/checkout/administration\n";
    manifest
        .insert(
            git.clone(),
            super::super::manifest_v2::EntryV2::regular(
                crate::blob_store::v2::ContentHash::of(bytes),
                bytes.len() as u64,
                super::super::manifest_v2::EntryPlanes::default(),
            ),
        )
        .unwrap();
    let snapshot = LiveDelta::new(Arc::new(ComposedGeneration::new(
        "git-file-builder",
        manifest,
        None,
    )))
    .snapshot()
    .with_install_cut(runtime.installed().install_cut().unwrap().clone());
    runtime
        .driver()
        .install(
            runtime.access(),
            &snapshot,
            runtime.driver().revision(runtime.access()),
        )
        .unwrap();
    runtime.remember_base();
    let pin = lock(&runtime.base_ref_pin).as_ref().cloned().unwrap();
    assert!(pin.generation.flatten().unwrap().get(&git).is_none());
    assert!(pin
        .generation
        .flatten()
        .unwrap()
        .get(&super::super::RelPath::from_os_path(Path::new("src/alpha.rs")).unwrap())
        .is_some());
}

#[cfg(unix)]
#[test]
fn base_ref_changed_stat_is_not_stable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.rs");
    std::fs::write(&path, "fn before() {}\n").unwrap();
    let before = std::fs::metadata(&path).unwrap();
    assert!(base_ref_stable(&before, &before));
    std::fs::write(&path, "fn after_and_larger() {}\n").unwrap();
    assert!(!base_ref_stable(
        &before,
        &std::fs::metadata(&path).unwrap()
    ));
}

#[test]
fn base_ref_is_not_a_configuration_key() {
    assert!(!include_str!("../../../../assets/aft.schema.json").contains("base_ref_prototype"));
    let defaults = serde_json::to_value(crate::config::Config::default()).unwrap();
    assert!(defaults.get("base_ref_prototype").is_none());
}

#[cfg(unix)]
#[test]
fn base_ref_identity_mismatch_takes_strict_path() {
    let storage = tempfile::tempdir().unwrap();
    let other_storage = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    write_tree(main.path(), FILES);
    write_tree(worker.path(), FILES);
    let base = base_ref_runtime(storage.path(), "main", main.path());
    refresh(&base, &Model::default());
    let pin = lock(&base.base_ref_pin).as_ref().cloned().unwrap();
    assert!(pin.compatible(storage.path(), "family", &producer().id()));
    for mismatch in ["storage", "family", "producer"] {
        assert!(
            !pin.compatible(
                if mismatch == "storage" {
                    other_storage.path()
                } else {
                    storage.path()
                },
                if mismatch == "family" {
                    "other"
                } else {
                    "family"
                },
                &if mismatch == "producer" {
                    SemanticProducer::current("other-model", EmbedTextCaps::default()).id()
                } else {
                    producer().id()
                },
            ),
            "{mismatch}"
        );
        let mut runtime = CheckoutSemantic::new(
            &crate::root_cache::StoragePaths::same(if mismatch == "storage" {
                other_storage.path()
            } else {
                storage.path()
            }),
            if mismatch == "family" {
                "other"
            } else {
                "family"
            },
            mismatch,
            worker.path(),
            if mismatch == "producer" {
                SemanticProducer::current("other-model", EmbedTextCaps::default())
            } else {
                producer()
            },
            Weak::new(),
        )
        .unwrap();
        runtime.base_ref_prototype = true;
        super::super::io::BASE_REF_PROTOTYPE_USERS.fetch_add(1, Ordering::Relaxed);
        runtime.load().unwrap();
        assert!(
            !runtime
                .installed()
                .generation()
                .name()
                .starts_with("base-ref-"),
            "{mismatch}"
        );
    }
}

#[cfg(unix)]
#[test]
fn base_ref_edit_invalidates_and_refreshes_strictly() {
    let storage = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    write_tree(main.path(), FILES);
    write_tree(worker.path(), FILES);
    let model = Model::default();
    let base = base_ref_runtime(storage.path(), "main", main.path());
    refresh(&base, &model);
    let runtime = base_ref_runtime(storage.path(), "worker", worker.path());
    assert!(runtime
        .installed()
        .generation()
        .name()
        .starts_with("base-ref-"));
    let path = worker.path().join("src/alpha.rs");
    std::fs::write(&path, "pub fn changed_after_bind() -> u32 { 29 }\n").unwrap();
    runtime.driver().record_absolute_change(&path);
    assert!(!query(&runtime, "alpha").complete());
    refresh(&runtime, &model);
    assert!(!runtime
        .installed()
        .generation()
        .name()
        .starts_with("base-ref-"));
    assert_eq!(
        rows(
            worker.path(),
            &query(&runtime, "changed after bind").results
        ),
        cold(worker.path(), "changed after bind").0
    );
}

#[cfg(unix)]
#[test]
fn base_ref_active_prebind_write_waits_even_without_begin_notification() {
    let storage = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    write_tree(main.path(), FILES);
    write_tree(worker.path(), FILES);
    let base = base_ref_runtime(storage.path(), "main", main.path());
    refresh(&base, &Model::default());
    // The root already exists, but its new semantic driver is not listening
    // when the write begins. The root-wide active count must still fence bind.
    let delta = Arc::new(Mutex::new(LiveDelta::new(Arc::clone(
        base.installed().generation(),
    ))));
    super::super::intent::register(worker.path(), &delta);
    let path = worker.path().join("src/alpha.rs");
    let writing = super::super::intent::record_paths([path.as_path()]);
    assert!(super::super::intent::active(worker.path()));
    let mut runtime = CheckoutSemantic::new(
        &crate::root_cache::StoragePaths::same(storage.path()),
        "family",
        "worker",
        worker.path(),
        producer(),
        Weak::new(),
    )
    .unwrap();
    runtime.base_ref_prototype = true;
    super::super::io::BASE_REF_PROTOTYPE_USERS.fetch_add(1, Ordering::Relaxed);
    runtime.driver().register_write_intent();
    assert!(
        runtime.load().is_err(),
        "an active write was declared ready"
    );
    assert_eq!(runtime.installed().generation().name(), "empty");
    drop(writing);
    assert!(!super::super::intent::active(worker.path()));
    runtime.load().unwrap();
    assert!(runtime
        .installed()
        .generation()
        .name()
        .starts_with("base-ref-"));
}

#[cfg(unix)]
#[test]
fn base_ref_kill_child() {
    let Ok(subject) = std::env::var("AFT_BASE_REF_KILL_SUBJECT") else {
        return;
    };
    let subject = PathBuf::from(subject);
    let storage = subject.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(storage.join("views-harness.marker"), b"test\n").unwrap();
    let main = subject.join("main");
    let worker = subject.join("worker");
    write_tree(&main, FILES);
    write_tree(&worker, FILES);
    let model = Model::default();
    let base = open(&storage, "main", &main);
    refresh(&base, &model);
    let runtime = open(&storage, "worker", &worker);
    refresh(&runtime, &model);
    let borrowed = runtime
        .installed()
        .generation()
        .name()
        .starts_with("base-ref-");
    assert_eq!(borrowed, std::env::var(BASE_REF_ENV).as_deref() == Ok("1"));
    let answer = query(&runtime, "beta cache insert");
    assert!(answer.complete());
    assert!(!answer.results.is_empty());
    std::fs::write(subject.join("ready"), b"semantic query served\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        std::hint::black_box((&base, &runtime, &answer));
    }
    panic!("kill-test parent did not send SIGKILL within 30 seconds");
}

#[cfg(unix)]
#[derive(Debug)]
struct BaseRefKilledInventory {
    registry: std::collections::BTreeMap<String, usize>,
    protections: std::collections::BTreeMap<String, usize>,
    worker_files: Vec<PathBuf>,
}

#[cfg(unix)]
fn base_ref_normalize(value: &mut serde_json::Value, subject: &Path) {
    match value {
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                if [
                    "owner_pid",
                    "owner_start",
                    "pid",
                    "start_time",
                    "hostname",
                    "created_at",
                    "renewed_at",
                    "created_at_ms",
                    "registered_at_ms",
                    "last_bind_ms",
                    "last_publish_ms",
                    "opened_at_ms",
                    "acquired_at_ms",
                    "measured_at_ms",
                    "reader_id",
                    "session_id",
                    "generation",
                ]
                .contains(&name.as_str())
                {
                    *value = serde_json::json!("<run-specific>");
                } else {
                    base_ref_normalize(value, subject);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                base_ref_normalize(item, subject);
            }
        }
        serde_json::Value::String(text) => {
            *text = text.replace(subject.to_str().unwrap(), "<subject>");
        }
        _ => {}
    }
}

#[cfg(unix)]
fn base_ref_files(path: &Path) -> Vec<PathBuf> {
    if !path.exists() {
        return Vec::new();
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            files.extend(base_ref_files(&entry.path()));
        } else {
            files.push(entry.path());
        }
    }
    files.sort();
    files
}

#[cfg(unix)]
fn base_ref_killed_inventory(subject: &Path, prototype: bool) -> BaseRefKilledInventory {
    use std::os::unix::process::ExitStatusExt;
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "views::semantic_runtime::tests::base_ref_kill_child",
            "--nocapture",
        ])
        .env("AFT_BASE_REF_KILL_SUBJECT", subject);
    if prototype {
        command.env(BASE_REF_ENV, "1");
    } else {
        command.env_remove(BASE_REF_ENV);
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !subject.join("ready").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "kill-test child exited before serving a query"
        );
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("kill-test child did not become ready within 30 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(9));
    // Open only after SIGKILL, before any recovery or GC pass. This never
    // inspects a live SQLite file through a second descriptor.
    let connection = rusqlite::Connection::open_with_flags(
        subject.join("storage/blobs/v2/family/members.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let tables = connection.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap()
        .query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    let mut registry = std::collections::BTreeMap::new();
    for table in tables {
        registry.insert(format!("table:{table}"), 1);
        let sql = format!("SELECT * FROM \"{}\"", table.replace('"', "\"\""));
        let mut statement = connection.prepare(&sql).unwrap();
        let columns = statement
            .column_names()
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        let rows = statement
            .query_map([], |row| {
                let mut fields = serde_json::Map::new();
                for (index, name) in columns.iter().enumerate() {
                    use rusqlite::types::ValueRef;
                    let value = match row.get_ref(index)? {
                        ValueRef::Null => serde_json::Value::Null,
                        ValueRef::Integer(value) => serde_json::json!(value),
                        ValueRef::Real(value) => serde_json::json!(value),
                        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
                            match std::str::from_utf8(bytes) {
                                Ok(text) => serde_json::json!(text),
                                Err(_) => serde_json::json!(bytes),
                            }
                        }
                    };
                    fields.insert(name.clone(), value);
                }
                Ok(serde_json::Value::Object(fields))
            })
            .unwrap();
        for row in rows {
            let mut row = row.unwrap();
            // Publication timestamps may be NULL for a reference adopter,
            // which intentionally never publishes a worker generation.
            base_ref_normalize(&mut row, subject);
            *registry
                .entry(format!("{table}:{}", serde_json::to_string(&row).unwrap()))
                .or_insert(0) += 1;
        }
    }
    drop(connection);
    let storage = subject.join("storage");
    BaseRefKilledInventory {
        registry,
        protections: base_ref_protection_inventory(&storage),
        worker_files: base_ref_files(
            &super::super::registry::view_dir(&storage, "worker").unwrap(),
        ),
    }
}

#[cfg(unix)]
fn base_ref_protection_inventory(storage: &Path) -> std::collections::BTreeMap<String, usize> {
    let mut inventory = std::collections::BTreeMap::new();
    for scope in std::fs::read_dir(storage.join("views/v2")).unwrap() {
        let scope = scope.unwrap();
        if !scope.file_type().unwrap().is_dir() {
            continue;
        }
        let name = scope.file_name().to_string_lossy().into_owned();
        for kind in ["pins", "readers"] {
            let directory = scope.path().join(kind);
            for file in base_ref_files(&directory) {
                let relative = file.strip_prefix(&directory).unwrap();
                let filename = file.file_name().unwrap().to_string_lossy();
                let temporary = filename.starts_with('.') || filename.contains(".tmp.");
                let canonical_path = if kind == "readers" && !temporary {
                    format!("{name}/readers/<generation>/<holder>.json")
                } else if kind == "pins" && filename.starts_with("live-") && !temporary {
                    format!(
                        "{name}/pins/<live>.{}",
                        file.extension().unwrap().to_string_lossy()
                    )
                } else {
                    format!("{name}/{kind}/{}", relative.display())
                };
                let bytes = std::fs::read(&file).unwrap();
                let canonical = match serde_json::from_slice::<serde_json::Value>(&bytes) {
                    Ok(mut value) => {
                        base_ref_normalize(&mut value, storage.parent().unwrap());
                        serde_json::to_vec(&value).unwrap()
                    }
                    Err(_) => bytes,
                };
                let key = format!("{canonical_path}:{}", blake3::hash(&canonical).to_hex());
                *inventory.entry(key).or_insert(0) += 1;
            }
        }
    }
    inventory
}

#[cfg(unix)]
fn base_ref_no_extra_protection(
    control: &std::collections::BTreeMap<String, usize>,
    candidate: &std::collections::BTreeMap<String, usize>,
) -> bool {
    candidate
        .iter()
        .all(|(key, count)| *count <= control.get(key).copied().unwrap_or(0))
        && !candidate.keys().any(|key| key.starts_with("worker/"))
}

#[cfg(unix)]
#[test]
fn base_ref_sigkill_leaves_no_extra_rows_or_protection() {
    let control = tempfile::tempdir().unwrap();
    let prototype = tempfile::tempdir().unwrap();
    let control_inventory = base_ref_killed_inventory(control.path(), false);
    let prototype_inventory = base_ref_killed_inventory(prototype.path(), true);
    let members = |inventory: &BaseRefKilledInventory| {
        inventory
            .registry
            .iter()
            .filter(|(key, _)| key.starts_with("members:"))
            .map(|(key, count)| (key.clone(), *count))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    assert_eq!(members(&prototype_inventory), members(&control_inventory));
    assert!(prototype_inventory.registry.iter().all(|(key, count)| *count <= control_inventory.registry.get(key).copied().unwrap_or(0)), "control={control_inventory:?} prototype={prototype_inventory:?}");
    assert!(
        base_ref_no_extra_protection(
            &control_inventory.protections,
            &prototype_inventory.protections
        ),
        "control={control_inventory:?} prototype={prototype_inventory:?}"
    );
    assert!(
        prototype_inventory.worker_files.is_empty(),
        "the reference adopter wrote durable files: {:?}",
        prototype_inventory.worker_files
    );
    assert!(!prototype.path().join("storage/blobs/v3").exists());
    let pins = super::super::registry::view_dir(&prototype.path().join("storage"), "worker")
        .unwrap()
        .join("pins");
    std::fs::create_dir_all(&pins).unwrap();
    // A synthetic extra pin must fail the same inventory comparison used
    // immediately after the real process kill.
    std::fs::write(pins.join("leaked.json"), b"leaked reference\n").unwrap();
    let leaked = base_ref_protection_inventory(&prototype.path().join("storage"));
    assert!(
        !base_ref_no_extra_protection(&control_inventory.protections, &leaked),
        "a deliberately leaked pin escaped detection"
    );
}

/// Real-model parity reuses a quiescent harness store, never a live daemon's
/// SQLite files. Only the small pre-bind edit may need a new embedding.
#[cfg(unix)]
#[test]
#[ignore = "requires the pinned corpus and real-model cache from the performance harness"]
fn base_ref_real_model_parity_including_prebind_edit() {
    let _ = env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .try_init();
    let required = |name| PathBuf::from(std::env::var(name).expect(name));
    let storage = required("AFT_VIEWS_PARITY_STORAGE");
    let main = required("AFT_VIEWS_PARITY_MAIN");
    let worker = required("AFT_VIEWS_PARITY_WORKER");
    let family = std::env::var("AFT_VIEWS_PARITY_FAMILY").unwrap();
    let config = crate::config::SemanticBackendConfig::default();
    let mut model = crate::semantic_index::EmbeddingModel::from_config(&config).unwrap();
    let fingerprint = model.fingerprint_for_view(&config).unwrap();
    let producer = SemanticProducer::current(fingerprint.as_string(), fingerprint.embed_text_caps);
    let open = |scope: &str, root: &Path, prototype: bool| {
        let mut runtime = CheckoutSemantic::new(
            &crate::root_cache::StoragePaths::same(&storage),
            &family,
            scope,
            root,
            producer.clone(),
            Weak::new(),
        )
        .unwrap();
        if prototype && !runtime.base_ref_prototype {
            runtime.base_ref_prototype = true;
            super::super::io::BASE_REF_PROTOTYPE_USERS.fetch_add(1, Ordering::Relaxed);
        }
        runtime.driver().register_write_intent();
        runtime.load().unwrap();
        runtime
    };
    let base = open("real-parity-main", &main, true);
    assert!(
        base.plane()
            .pending_work(base.access(), &base.installed())
            .is_empty(),
        "the real-model store must be prewarmed under the same producer"
    );
    base.refresh(FillBudget::default(), &mut |texts| model.embed(texts))
        .unwrap();
    assert!(
        lock(&base.base_ref_pin).is_some(),
        "the prewarmed strict snapshot was not remembered as a ready prototype base"
    );
    struct RestoreFile(PathBuf, Vec<u8>);
    impl Drop for RestoreFile {
        fn drop(&mut self) {
            std::fs::write(&self.0, &self.1).unwrap();
        }
    }
    let probe = worker.join("views_perf_probe.rs");
    let restore = RestoreFile(probe.clone(), std::fs::read(&probe).unwrap());
    let mut comparisons = 0;
    for modified in [false, true] {
        let suffix = if modified { "edited" } else { "clean" };
        if modified {
            let changed = String::from_utf8(restore.1.clone()).unwrap().replace(
                "{ probe_alpha() } // TODO VIEWS_PERF_STATE_ALPHA",
                "{ probe_beta() } // TODO VIEWS_PERF_STATE_BETA",
            );
            assert_ne!(changed.as_bytes(), restore.1);
            std::fs::write(&probe, changed).unwrap();
        }
        let strict = open(&format!("real-strict-{suffix}"), &worker, false);
        assert!(
            strict
                .plane()
                .pending_work(strict.access(), &strict.installed())
                .len()
                <= 1
        );
        strict
            .refresh(FillBudget::default(), &mut |texts| model.embed(texts))
            .unwrap();
        let prototype = open(&format!("real-prototype-{suffix}"), &worker, true);
        assert_eq!(
            prototype
                .installed()
                .generation()
                .name()
                .starts_with("base-ref-"),
            !modified
        );
        assert!(
            prototype
                .plane()
                .pending_work(prototype.access(), &prototype.installed())
                .len()
                <= 1
        );
        prototype
            .refresh(FillBudget::default(), &mut |texts| model.embed(texts))
            .unwrap();
        for text in [
            "function probe_entry in views_perf_probe.rs",
            "how are view generations published",
            "how does semantic vector search score candidates",
        ] {
            let query_vector = model.embed(vec![text.to_owned()]).unwrap().remove(0);
            for top_k in [1, 3, 20] {
                let expected = strict.search(&query_vector, top_k, &|_| true).unwrap();
                let actual = prototype.search(&query_vector, top_k, &|_| true).unwrap();
                assert!(expected.complete() && actual.complete());
                assert_eq!(
                    rows(&worker, &actual.results),
                    rows(&worker, &expected.results),
                    "state={suffix} query={text} top_k={top_k}"
                );
                comparisons += 1;
            }
        }
    }
    assert_eq!(comparisons, 18);
    eprintln!("REAL_MODEL_BASE_REF_PARITY: 18 exact top-k comparisons passed on one worker, including a pre-bind edit");
}

use super::super::first_load::MembershipWalker;

/// Two worktrees of one family with identical content embed each chunk once
/// between them; a later session on the same worktrees embeds nothing and
/// ranks exactly like a legacy build of the same content.
#[test]
fn two_worktrees_two_sessions_embed_each_chunk_once() {
    let storage = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_tree(first.path(), FILES);
    write_tree(second.path(), FILES);
    let (expected, chunks) = cold(first.path(), "beta cache insert");
    assert!(chunks > 0);

    let model = Model::default();
    {
        let a = open(storage.path(), "worktree-a", first.path());
        let b = open(storage.path(), "worktree-b", second.path());
        assert!(Arc::ptr_eq(a.plane(), b.plane()), "one plane per storage");
        refresh(&a, &model);
        refresh(&b, &model);
        assert_eq!(model.texts(), chunks, "identical content embedded twice");
        let answer = query(&b, "beta cache insert");
        assert!(answer.complete(), "second worktree incomplete: {answer:?}");
        assert_eq!(rows(second.path(), &answer.results), expected);
    }

    // Session 2: the first session's checkouts and plane are gone.
    let embedded = model.texts();
    let a = open(storage.path(), "worktree-a", first.path());
    let b = open(storage.path(), "worktree-b", second.path());
    refresh(&a, &model);
    refresh(&b, &model);
    assert_eq!(model.texts(), embedded, "a later session embedded again");
    for (runtime, root) in [(&a, first.path()), (&b, second.path())] {
        let answer = query(runtime, "beta cache insert");
        assert!(answer.complete(), "incomplete after restart: {answer:?}");
        assert_eq!(rows(root, &answer.results), expected);
    }
}

#[test]
fn family_fill_rechecks_completed_work_after_acquiring_claim() {
    let storage = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let files = [("src/file.rs", "pub fn one_chunk() -> u8 { 1 }\n")];
    write_tree(first.path(), &files);
    write_tree(second.path(), &files);
    let a = open(storage.path(), "a", first.path());
    let b = Arc::new(open(storage.path(), "b", second.path()));
    let model = Arc::new(Model::default());
    let callback_model = Arc::clone(&model);
    let callback_b = Arc::clone(&b);
    // Finish the competing fill exactly between a's store miss and claim.
    // No sleep or host scheduling assumption is involved.
    super::super::semantic::before_claim_for_test(move || {
        let report = refresh(&callback_b, &callback_model);
        assert_eq!((report.embedded_keys, report.installed), (1, 1));
    });
    let report = refresh(&a, &model);
    let usage = a
        .owner
        .open_store(crate::blob_store::v2::FamilyPlane::Semantic)
        .unwrap()
        .usage()
        .unwrap();
    eprintln!(
        "completed claim race: texts={} calls={} outer_embedded={} stored_rows={}",
        model.texts(),
        model.calls(),
        report.embedded_keys,
        usage.rows
    );
    assert!(query(&a, "one chunk").complete());
    assert_eq!(
        rows(first.path(), &query(&a, "one chunk").results),
        cold(first.path(), "one chunk").0
    );
    assert_eq!(
        model.texts(),
        1,
        "a completed claim must not embed the same stored key again"
    );
    assert_eq!(model.calls(), 1);
    assert_eq!(report.embedded_keys, 0);
    // One canonical file run, one reusable chunk vector, and the marker row
    // that vouches for the run's vectors matching that chunk row.
    assert_eq!(usage.rows, ROWS_PER_ONE_CHUNK_FILE);
    assert_eq!(
        a.installed()
            .generation()
            .flatten()
            .unwrap()
            .to_json_bytes()
            .unwrap(),
        b.installed()
            .generation()
            .flatten()
            .unwrap()
            .to_json_bytes()
            .unwrap()
    );
    assert_eq!(
        rows(first.path(), &query(&a, "one chunk").results),
        rows(second.path(), &query(&b, "one chunk").results)
    );
}

#[test]
fn completed_embedding_batches_survive_a_later_batch_failure() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(
        root.path(),
        &[
            ("a.rs", "pub fn first_chunk() -> u8 { 1 }\n"),
            ("b.rs", "pub fn second_chunk() -> u8 { 2 }\n"),
            ("c.rs", "pub fn third_chunk() -> u8 { 3 }\n"),
        ],
    );
    let runtime = open(storage.path(), "batch-failure", root.path());
    let model = Model::default();
    let budget = FillBudget {
        max_batch: 1,
        ..FillBudget::default()
    };
    let report = runtime
        .refresh(budget, &mut |texts| {
            let vectors = model.embed(texts)?;
            if model.calls() == 2 {
                Err("embedding backend interrupted".into())
            } else {
                Ok(vectors)
            }
        })
        .unwrap();
    let store = runtime
        .owner
        .open_store(crate::blob_store::v2::FamilyPlane::Semantic)
        .unwrap();
    eprintln!(
        "later batch failure: texts={} calls={} embedded_keys={} installed={} rows={}",
        model.texts(),
        model.calls(),
        report.embedded_keys,
        report.installed,
        store.usage().unwrap().rows
    );
    assert_eq!(report.errors.len(), 1);
    assert_eq!(
        (report.embedded_keys, report.installed),
        (1, 1),
        "a later batch error must not discard an already complete file run"
    );
    assert_eq!(store.usage().unwrap().rows, ROWS_PER_ONE_CHUNK_FILE);
    runtime
        .refresh(budget, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!(
        model.texts(),
        4,
        "retry must not re-embed the completed first file"
    );
    assert_eq!(model.calls(), 4);
    assert_eq!(store.usage().unwrap().rows, 3 * ROWS_PER_ONE_CHUNK_FILE);
    let answer = query(&runtime, "chunk");
    assert!(answer.complete());
    assert_eq!(
        rows(root.path(), &answer.results),
        cold(root.path(), "chunk").0
    );
}

#[test]
fn repeated_worktree_edits_with_periodic_backend_failures_never_repeat_stored_texts() {
    let storage = tempfile::tempdir().unwrap();
    let roots: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    for root in &roots {
        for i in 0..2048 {
            std::fs::write(
                root.path().join(format!("file_{i:04}.rs")),
                format!("pub fn item_{i:04}() -> u32 {{ {i} }}\n"),
            )
            .unwrap();
        }
    }
    let runtimes: Vec<_> = roots
        .iter()
        .enumerate()
        .map(|(i, root)| open(storage.path(), &format!("flaky-worktree-{i}"), root.path()))
        .collect();
    let store = runtimes[0]
        .owner
        .open_store(crate::blob_store::v2::FamilyPlane::Semantic)
        .unwrap();
    let model = Model::default();
    let completed = std::cell::RefCell::new(std::collections::HashSet::new());
    let successful_texts = std::cell::Cell::new(0usize);
    let mut failed_texts = 0usize;
    let mut embed = |texts: Vec<String>| {
        // One chunk per file produces a file row, a chunk-vector row and a
        // verified-run marker row. Each completed request must persist them
        // before another request is sent.
        assert_eq!(
            store.usage().unwrap().rows as usize,
            successful_texts.get() * ROWS_PER_ONE_CHUNK_FILE as usize,
            "completed batches were not persisted before the next request"
        );
        for text in &texts {
            assert!(
                !completed.borrow().contains(text),
                "a stored text was embedded again after retry"
            );
        }
        let count = texts.len();
        let retained = texts.clone();
        let vectors = model.embed(texts)?;
        if model.calls().is_multiple_of(7) {
            failed_texts += count;
            Err("embedding backend interrupted every seventh batch".into())
        } else {
            completed.borrow_mut().extend(retained);
            successful_texts.set(successful_texts.get() + count);
            Ok(vectors)
        }
    };
    let budget = FillBudget {
        max_files: 4096,
        ..FillBudget::default()
    };
    let mut new_keys = 0;
    for runtime in &runtimes {
        let before = model.calls();
        for attempt in 0..64 {
            let report = runtime.refresh(budget, &mut embed).unwrap();
            new_keys += report.embedded_keys;
            if report.errors.is_empty() && report.deferred == 0 {
                break;
            }
            assert!(attempt < 63, "fixture failed to settle");
        }
        assert!(query(runtime, "item").complete());
        if runtime.access.scope() != "flaky-worktree-0" {
            assert_eq!(
                model.calls(),
                before,
                "a sibling must reuse all stored vectors"
            );
        }
    }
    for round in 0..8 {
        // Change the signature so each revision has genuinely new embed text;
        // body-only edits of these single-line summaries reuse the vector.
        let source = format!(
            "pub fn revised_item_{round}() -> u32 {{ {} }}\n",
            round + 9000
        );
        for (i, (runtime, root)) in runtimes.iter().zip(&roots).enumerate() {
            let file = root.path().join("file_0000.rs");
            std::fs::write(&file, &source).unwrap();
            runtime.driver.record_absolute_change(&file);
            let before = model.calls();
            for attempt in 0..64 {
                let report = runtime.refresh(budget, &mut embed).unwrap();
                new_keys += report.embedded_keys;
                if report.errors.is_empty() && report.deferred == 0 {
                    break;
                }
                assert!(attempt < 63, "fixture failed to settle");
            }
            if i > 0 {
                assert_eq!(
                    model.calls(),
                    before,
                    "same-content sibling edit sent a model request"
                );
            }
            assert!(query(runtime, "revised item").complete());
        }
    }
    drop(embed);
    assert_eq!(successful_texts.get(), 2056);
    assert_eq!(new_keys, 2056);
    assert_eq!(store.usage().unwrap().rows, 2056 * ROWS_PER_ONE_CHUNK_FILE);
    assert_eq!(failed_texts, 321);
    assert_eq!(model.texts(), 2377);
    assert_eq!(model.calls(), 46);
    for (runtime, root) in runtimes.iter().zip(&roots) {
        assert_eq!(
            rows(root.path(), &query(runtime, "revised item").results),
            cold(root.path(), "revised item").0
        );
    }
    eprintln!("periodic failure, three worktrees: successful texts={} failed texts={failed_texts} sent texts={} calls={} new keys={new_keys} stored rows={}", successful_texts.get(), model.texts(), model.calls(), store.usage().unwrap().rows);
}

#[test]
fn repeated_worktree_edits_embed_only_new_family_keys_even_after_dropped_admission() {
    let storage = tempfile::tempdir().unwrap();
    let roots: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    for root in &roots {
        for i in 0..2048 {
            std::fs::write(
                root.path().join(format!("file_{i:04}.rs")),
                format!("pub fn item_{i:04}() -> u32 {{ {i} }}\n"),
            )
            .unwrap();
        }
    }
    let runtimes: Vec<_> = roots
        .iter()
        .enumerate()
        .map(|(i, root)| open(storage.path(), &format!("worktree-{i}"), root.path()))
        .collect();
    let model = Model::default();
    let budget = FillBudget {
        max_files: 4096,
        ..FillBudget::default()
    };
    let mut embedded_keys = 0;
    for (i, runtime) in runtimes.iter().enumerate() {
        let report = runtime
            .refresh(budget, &mut |texts| model.embed(texts))
            .unwrap();
        embedded_keys += report.embedded_keys;
        assert_eq!(report.embedded_keys, if i == 0 { 2048 } else { 0 });
        assert_eq!(report.model_calls, if i == 0 { 32 } else { 0 });
    }
    let store = runtimes[0]
        .owner
        .open_store(crate::blob_store::v2::FamilyPlane::Semantic)
        .unwrap();
    for runtime in &runtimes[1..] {
        assert_eq!(
            runtime
                .owner
                .open_store(crate::blob_store::v2::FamilyPlane::Semantic)
                .unwrap()
                .path(),
            store.path()
        );
    }
    for round in 0..8 {
        // Each revision must change embedded text, not only whole-file bytes.
        let source = format!(
            "pub fn revised_item_{round}() -> u32 {{ {} }}\n",
            round + 9000
        );
        for (i, (runtime, root)) in runtimes.iter().zip(&roots).enumerate() {
            let file = root.path().join("file_0000.rs");
            {
                let _intent = crate::views::intent::record_paths([file.as_path()]);
                std::fs::write(&file, &source).unwrap();
            }
            let before = model.texts();
            let report = runtime
                .refresh(budget, &mut |texts| model.embed(texts))
                .unwrap();
            embedded_keys += report.embedded_keys;
            assert_eq!(report.embedded_keys, usize::from(i == 0));
            assert_eq!(report.model_calls, usize::from(i == 0));
            assert_eq!(
                model.texts() - before,
                usize::from(i == 0),
                "second/third worktree repeated an embedding"
            );
            // Repeated no-op writes to the same content must not send texts.
            let _intent = crate::views::intent::record_paths([file.as_path()]);
            std::fs::write(&file, &source).unwrap();
            drop(_intent);
            let report = runtime
                .refresh(budget, &mut |texts| model.embed(texts))
                .unwrap();
            assert_eq!((report.embedded_keys, report.model_calls), (0, 0));
        }
    }
    assert_eq!(model.texts(), 2056);
    assert_eq!(model.calls(), 40);
    assert_eq!(embedded_keys, 2056);
    assert_eq!(store.usage().unwrap().rows, 2056 * ROWS_PER_ONE_CHUNK_FILE);

    // A source completion rejected at admission still stores its immutable
    // payload first. Retry and a sibling must reuse it, not re-embed it.
    let a = &runtimes[0];
    let source = "pub fn admission_was_cancelled() -> u32 { 99999 }\n";
    let file = roots[0].path().join("file_0000.rs");
    std::fs::write(&file, source).unwrap();
    a.driver.record_absolute_change(&file);
    let snapshot = a
        .loader
        .refresh(&a.access, Arc::clone(a.installed().generation()))
        .unwrap();
    let path = super::super::RelPath::new(b"file_0000.rs".to_vec()).unwrap();
    let mut moved = super::super::snapshot::LiveDelta::new(Arc::clone(snapshot.generation()));
    moved.apply(
        path,
        super::super::snapshot::LiveEntry::new(
            super::super::snapshot::DiskState::of_bytes(b"other bytes"),
            1,
        ),
    );
    let cancelled = moved.snapshot();
    let report = a
        .plane
        .fill(
            &a.owner,
            &snapshot,
            budget,
            &mut |texts| model.embed(texts),
            &|| cancelled.clone(),
        )
        .unwrap();
    assert_eq!(
        (report.embedded_keys, report.installed, report.dropped),
        (1, 0, 1)
    );
    let before = model.texts();
    let retry = a.refresh(budget, &mut |texts| model.embed(texts)).unwrap();
    assert_eq!(
        (retry.embedded_keys, retry.model_calls, retry.installed),
        (0, 0, 1)
    );
    let file = roots[1].path().join("file_0000.rs");
    std::fs::write(&file, source).unwrap();
    runtimes[1].driver.record_absolute_change(&file);
    let sibling = runtimes[1]
        .refresh(budget, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!((sibling.embedded_keys, sibling.model_calls), (0, 0));
    assert_eq!(model.texts(), before);
    assert_eq!(model.texts(), 2057);
    assert_eq!(model.calls(), 41);
    assert_eq!(store.usage().unwrap().rows, 2057 * ROWS_PER_ONE_CHUNK_FILE);
    for (runtime, root) in runtimes.iter().zip(&roots) {
        let answer = query(runtime, "revised item");
        assert!(answer.complete());
        assert_eq!(
            rows(root.path(), &answer.results),
            cold(root.path(), "revised item").0
        );
    }
    eprintln!("three worktrees, repeated edits and cancelled admission: texts={} calls={} stored_rows={} payload_bytes={}", model.texts(), model.calls(), store.usage().unwrap().rows, store.usage().unwrap().payload_bytes);
}

/// An edit the watcher reports and an edit AFT writes each reach the fill:
/// the new content is embedded and the next query scores it.
#[test]
fn watcher_and_aft_edits_reach_the_fill() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    write_tree(&root_path, FILES);
    let runtime = open(storage.path(), "scope", &root_path);
    let model = Model::default();
    refresh(&runtime, &model);

    // Outside edit: the watcher drain hands the path to the driver.
    let before = model.texts();
    write_tree(
        &root_path,
        &[(
            "src/gamma.rs",
            "pub fn gamma_watched_rewrite() -> u8 {\n    3\n}\n",
        )],
    );
    runtime
        .driver()
        .record_absolute_change(&root_path.join("src/gamma.rs"));
    let pending = query(&runtime, "gamma watched rewrite");
    assert!(
        pending.pending.contains(&root_path.join("src/gamma.rs")),
        "an unreconciled edit must be a named gap: {pending:?}"
    );
    let report = refresh(&runtime, &model);
    assert!(report.embedded_keys >= 1 && model.texts() > before);
    let answer = query(&runtime, "gamma watched rewrite");
    assert!(answer.complete(), "{answer:?}");
    assert_eq!(
        rows(&root_path, &answer.results),
        cold(&root_path, "gamma watched rewrite").0
    );

    // AFT write: the write-intent listener records it before the bytes land.
    let before = model.texts();
    let target = root_path.join("src/alpha.rs");
    {
        let _intent = crate::views::intent::record_paths([target.as_path()]);
        std::fs::write(&target, "pub fn alpha_written_by_aft() -> u8 {\n    1\n}\n").unwrap();
    }
    assert!(
        query(&runtime, "alpha written").pending.contains(&target),
        "an acknowledged write must be a named gap until reconciled"
    );
    refresh(&runtime, &model);
    assert!(model.texts() > before, "the AFT write was never embedded");
    let answer = query(&runtime, "alpha written by aft");
    assert!(answer.complete(), "{answer:?}");
    assert_eq!(
        rows(&root_path, &answer.results),
        cold(&root_path, "alpha written by aft").0
    );
}

fn pending_semantic(runtime: &CheckoutSemantic) -> usize {
    runtime
        .installed()
        .generation()
        .flatten()
        .unwrap()
        .entries()
        .filter(|(_, entry)| {
            entry
                .plane_state(crate::blob_store::v2::FamilyPlane::Semantic)
                .is_some_and(|state| state.is_pending())
        })
        .count()
}

fn large_pin_fixture() -> (tempfile::TempDir, tempfile::TempDir, CheckoutSemantic) {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for i in 0..2048 {
        std::fs::write(
            root.path().join(format!("file_{i:04}.rs")),
            format!("pub fn item_{i:04}() -> u32 {{ {i} }}\n"),
        )
        .unwrap();
    }
    let runtime = open(storage.path(), "pins", root.path());
    (storage, root, runtime)
}

fn fill_large(runtime: &CheckoutSemantic) {
    let report = runtime
        .plane()
        .fill(
            &runtime.owner,
            &runtime.installed(),
            FillBudget {
                max_files: 4096,
                ..FillBudget::default()
            },
            &mut |texts| Model::default().embed(texts),
            &|| runtime.installed(),
        )
        .unwrap();
    assert_eq!(
        (
            report.installed,
            report.embedded_keys,
            report.embedded_texts
        ),
        (2048, 2048, 2048)
    );
}

#[test]
fn large_semantic_fill_batches_pin_writes() {
    use crate::pins::work_counters::{key_file_bytes, key_file_work};
    let (_storage, root, runtime) = large_pin_fixture();
    let (writes, syncs) = key_file_work();
    let bytes = key_file_bytes();
    fill_large(&runtime);
    let after = key_file_work();
    eprintln!(
        "fill pins: writes={} bytes={} key syncs={}",
        after.0 - writes,
        key_file_bytes() - bytes,
        after.1 - syncs
    );
    let answer = query(&runtime, "item");
    assert!(answer.complete());
    assert_eq!(
        rows(root.path(), &answer.results),
        cold(root.path(), "item").0
    );
    assert_eq!((after.0 - writes, after.1 - syncs), (1, 0));
    // File keys and chunk-vector keys share one linear pin write.
    assert_eq!(key_file_bytes() - bytes, 2048 * 2 * 65);
}

#[test]
fn large_semantic_materialization_batches_pins_and_touches() {
    use crate::pins::work_counters::{key_file_bytes, key_file_work};
    let (_storage, root, runtime) = large_pin_fixture();
    fill_large(&runtime);
    let answer = query(&runtime, "item");
    let (writes, syncs) = key_file_work();
    let bytes = key_file_bytes();
    crate::blob_store::v2::take_touch_transactions();
    let (snapshot, revision) = runtime.driver.installed_cut();
    runtime
        .loader
        .fold(&runtime.access, &snapshot, revision)
        .unwrap();
    let after = key_file_work();
    let transactions = crate::blob_store::v2::take_touch_transactions();
    let written_bytes = key_file_bytes() - bytes;
    eprintln!(
        "fold pins: writes={} bytes={} key syncs={} touch transactions={transactions}",
        after.0 - writes,
        written_bytes,
        after.1 - syncs
    );
    assert_eq!(pending_semantic(&runtime), 0);
    assert_eq!(
        rows(root.path(), &query(&runtime, "item").results),
        rows(root.path(), &answer.results)
    );
    let manifest = runtime
        .installed()
        .generation()
        .flatten()
        .unwrap()
        .to_json_bytes()
        .unwrap();
    let store = runtime
        .owner
        .open_store(crate::blob_store::v2::FamilyPlane::Semantic)
        .unwrap();
    let blobs: Vec<_> = runtime
        .installed()
        .generation()
        .flatten()
        .unwrap()
        .ready_keys()
        .map(|key| (key, store.get(&key).unwrap().unwrap()))
        .collect();
    runtime.load().unwrap();
    assert_eq!(
        runtime
            .installed()
            .generation()
            .flatten()
            .unwrap()
            .to_json_bytes()
            .unwrap(),
        manifest
    );
    for (key, payload) in blobs {
        assert_eq!(store.get(&key).unwrap().unwrap(), payload);
    }
    assert_eq!((after.0 - writes, after.1 - syncs), (2, 0));
    assert_eq!(written_bytes, 2048 * 65 * 2);
    // Every key was just stored in this epoch, so folding needs no writer.
    assert_eq!(transactions, 0);
}

/// A large, already-filled checkout must not read or embed unchanged sources
/// again just to publish one edit. Compare to the strict loader and an
/// independent cold index, not to another copy of the fast path.
#[test]
fn large_checkout_edit_counts_source_work_and_preserves_outputs() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root.path()).unwrap();
    for i in 0..2048 {
        let path = root.join(format!("file_{i:04}.rs"));
        std::fs::write(&path, format!("pub fn item_{i:04}() -> u32 {{ {i} }}\n")).unwrap();
        // Windows refuses to change times through a read-only handle.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(60)),
            )
            .unwrap();
    }
    let runtime = open(storage.path(), "large", &root);
    let model = Model::default();
    let budget = FillBudget {
        max_files: 4096,
        ..FillBudget::default()
    };
    runtime
        .refresh(budget, &mut |texts| model.embed(texts))
        .unwrap();
    runtime.driver().take_reconcile_work();
    let before = model.texts();
    let target = root.join("file_0000.rs");
    let changed = "pub fn rewritten_item() -> u32 { 9999 }\n";
    {
        let _intent = crate::views::intent::record_paths([target.as_path()]);
        std::fs::write(&target, changed).unwrap();
    }
    let report = runtime
        .refresh(budget, &mut |texts| model.embed(texts))
        .unwrap();
    let work = runtime.driver().take_reconcile_work();
    eprintln!(
        "edit: {work:?}; embedded texts={}, keys={}",
        model.texts() - before,
        report.embedded_keys
    );
    assert_eq!(
        model.texts() - before,
        1,
        "unchanged content was re-embedded"
    );
    let answer = query(&runtime, "rewritten item");
    assert!(answer.complete(), "{answer:?}");
    assert_eq!(
        rows(&root, &answer.results),
        cold(&root, "rewritten item").0
    );
    let snapshot = runtime.installed();
    let manifest = snapshot
        .generation()
        .flatten()
        .unwrap()
        .to_json_bytes()
        .unwrap();
    let store = runtime
        .owner
        .open_store(crate::blob_store::v2::FamilyPlane::Semantic)
        .unwrap();
    let blobs: Vec<_> = snapshot
        .generation()
        .flatten()
        .unwrap()
        .ready_keys()
        .map(|key| (key, store.get(&key).unwrap().unwrap()))
        .collect();
    runtime.load().unwrap();
    assert_eq!(
        runtime
            .installed()
            .generation()
            .flatten()
            .unwrap()
            .to_json_bytes()
            .unwrap(),
        manifest
    );
    for (key, payload) in blobs {
        assert_eq!(store.get(&key).unwrap().unwrap(), payload);
    }
    assert_eq!(
        rows(&root, &query(&runtime, "rewritten item").results),
        rows(&root, &answer.results)
    );
    assert_eq!(
        work.walks, 1,
        "an edit needs one strict membership reconciliation, not fold walks"
    );
    #[cfg(unix)]
    {
        assert_eq!(
            work.reads, 1,
            "unchanged files should cost a stat, not a source read"
        );
        assert_eq!(work.content_hash_bytes, changed.len());
        assert_eq!(work.attachments, 1);
        assert_eq!(
            work.stats, 2049,
            "one stat per member plus a post-read stability stat"
        );
    }
}

/// A catch-up that takes several budgeted rounds serves each round's vectors
/// at once but publishes them in one fold at the end, because every fold
/// re-walks the whole checkout.
#[test]
fn budgeted_rounds_fold_once_when_caught_up() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let runtime = open(storage.path(), "scope", root.path());
    let model = Model::default();
    let one_file = FillBudget {
        max_files: 1,
        ..FillBudget::default()
    };
    assert_eq!(pending_semantic(&runtime), 3);
    let first = runtime
        .refresh(one_file, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!((first.installed, first.deferred), (1, 2));
    let generation = runtime.installed().generation().name().to_owned();
    assert_eq!(pending_semantic(&runtime), 3, "folded before catching up");
    assert_eq!(query(&runtime, "alpha").pending.len(), 2);
    runtime
        .refresh(one_file, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!(runtime.installed().generation().name(), generation);
    let last = runtime
        .refresh(one_file, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!((last.installed, last.deferred), (1, 0));
    assert_eq!(pending_semantic(&runtime), 0, "caught up but never folded");
    assert!(query(&runtime, "alpha").complete());
}

fn fast_schedule(root: &Path) -> FillSchedule {
    FillSchedule {
        root: root.to_path_buf(),
        quiet_window: Duration::from_millis(10),
        retry_initial: Duration::from_millis(20),
        retry_max: Duration::from_millis(80),
        limiter: crate::cold_build_limiter::isolated_limiter(1),
        paused: Box::new(|| false),
        max_batch: 64,
    }
}

#[test]
fn small_chunk_reuse_fill_completes_with_cold_cap_held() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = "pub fn reused() -> u8 { 1 }\n";
    write_tree(root.path(), &[("file.rs", source)]);
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let model = Arc::new(Model::default());
    refresh(&runtime, &model);
    let embedded = model.texts();
    let path = root.path().join("file.rs");
    std::fs::write(&path, format!("\n\n{source}")).unwrap();
    runtime.driver().record_absolute_change(&path);
    runtime.load().unwrap();
    assert_eq!(pending_semantic(&runtime), 1);
    runtime.driver().take_reconcile_work();
    runtime.plane().take_source_reads();
    let schedule = fast_schedule(root.path());
    let cold = Arc::clone(&schedule.limiter);
    let held = schedule.limiter.try_acquire().unwrap();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let worker = {
        let weak = Arc::downgrade(&slot);
        let runtime = Arc::clone(&runtime);
        let model = Arc::clone(&model);
        std::thread::spawn(move || {
            super::super::semantic::before_claim_for_test(|| {
                std::thread::sleep(Duration::from_millis(30));
            });
            let (outcome, lines) = crate::logging::capture_log_lines(|| {
                catch_up(
                    &weak,
                    epoch,
                    &runtime,
                    &schedule,
                    FillBudget::default(),
                    &mut |texts| model.embed(texts),
                )
            });
            tx.send((matches!(outcome, FillOutcome::Settled), lines))
                .unwrap();
        })
    };
    // Decide by what the fill does, not by how long it takes: it either
    // finishes, or it queues on the held cold-build cap, which is the defect.
    // A slow machine only makes the loop run longer; the test runner's
    // overall timeout bounds a genuine hang elsewhere.
    let completed = loop {
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(result) => break Some(result),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break None,
        }
        if !cold.census().queued.is_empty() {
            slot.clear();
            break None;
        }
    };
    drop(held);
    worker.join().unwrap();
    let (settled, lines) = completed.expect("reuse-only fill waited for the cold-build cap");
    assert!(settled);
    assert_eq!(model.texts(), embedded);
    assert!(query(&runtime, "reused").complete());
    let work = runtime.driver().take_reconcile_work();
    assert_eq!(
        (work.walks, work.reads),
        (0, 0),
        "fold repeated source reconciliation: {work:?}"
    );
    assert_eq!(runtime.plane().take_source_reads().get(&path), Some(&1));
    let summary = lines
        .iter()
        .find(|line| line.contains("INFO semantic view fill"))
        .unwrap();
    assert!(
        summary.contains("chunk_reused=1") && summary.contains("calls=0"),
        "{summary}"
    );
    assert!(summary.contains("cold_admitted=false"), "{summary}");
    let lookup_ms: u128 = summary
        .split_whitespace()
        .find_map(|part| part.strip_prefix("file_lookup_ms="))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        lookup_ms >= 30,
        "lookup delay was not attributed: {summary}"
    );
    eprintln!(
        "{summary}; reconcile_walks={} reconcile_reads={} fill_reads=1",
        work.walks, work.reads
    );
}

/// Most fills in a running daemon are woken by an edit and start with that
/// edit still unreconciled. This test leaves such an edit in place before
/// `catch_up`. The edit only moved chunks whose vectors are stored, so the fill
/// embeds nothing and must finish without queueing for a cold-build slot.
#[test]
fn unreconciled_reuse_only_edit_completes_with_cold_cap_held() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = "pub fn reused_after_edit() -> u8 { 1 }\n";
    write_tree(root.path(), &[("file.rs", source)]);
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let model = Arc::new(Model::default());
    refresh(&runtime, &model);
    let embedded = model.texts();
    let path = root.path().join("file.rs");
    std::fs::write(&path, format!("\n\n{source}")).unwrap();
    runtime.driver().record_absolute_change(&path);
    assert!(
        super::needs_reload(&runtime.installed()),
        "the fixture must leave the edit unreconciled"
    );
    let schedule = fast_schedule(root.path());
    let cold = Arc::clone(&schedule.limiter);
    let held = schedule.limiter.try_acquire().unwrap();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let worker = {
        let weak = Arc::downgrade(&slot);
        let runtime = Arc::clone(&runtime);
        let model = Arc::clone(&model);
        std::thread::spawn(move || {
            let (outcome, lines) = crate::logging::capture_log_lines(|| {
                catch_up(
                    &weak,
                    epoch,
                    &runtime,
                    &schedule,
                    FillBudget::default(),
                    &mut |texts| model.embed(texts),
                )
            });
            tx.send((matches!(outcome, FillOutcome::Settled), lines))
                .unwrap();
        })
    };
    // Decide by what the fill does, not by how long it takes: it either
    // finishes, or it queues on the held cold-build cap, which is the defect.
    // The deadline only guards against a worker that stalls before doing
    // either. On expiry the lane is cleared and the held permit released
    // before the test fails; the worker stops at its next currency check.
    let deadline = Instant::now() + Duration::from_secs(60);
    let completed = loop {
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(result) => break Some(result),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break None,
        }
        if !cold.census().queued.is_empty() {
            slot.clear();
            break None;
        }
        if Instant::now() >= deadline {
            slot.clear();
            drop(held);
            panic!(
                "hang guard: the fill neither finished nor queued on the held \
                 cold-build cap within 60 s"
            );
        }
    };
    drop(held);
    worker.join().unwrap();
    let (settled, lines) =
        completed.expect("reuse-only fill with an unreconciled edit waited for the cold-build cap");
    assert!(settled);
    assert_eq!(model.texts(), embedded, "the edit only moved stored chunks");
    assert!(query(&runtime, "reused").complete());
    let summary = lines
        .iter()
        .find(|line| line.contains("INFO semantic view fill"))
        .unwrap();
    assert!(
        summary.contains("calls=0") && summary.contains("cold_admitted=false"),
        "{summary}"
    );
}

/// More unreconciled edits than the small-preparation file bound still take a
/// cold-build slot before the reconcile starts, as a first load does.
#[test]
fn many_unreconciled_edits_take_cold_slot_before_reconciling() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let files: Vec<(std::path::PathBuf, String)> = (0..=SMALL_PREPARATION_FILES)
        .map(|i| {
            (
                root.path().join(format!("file_{i}.rs")),
                format!("pub fn edited_{i}() {{}}\n"),
            )
        })
        .collect();
    for (path, source) in &files {
        std::fs::write(path, source).unwrap();
    }
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let model = Arc::new(Model::default());
    refresh(&runtime, &model);
    for (path, source) in &files {
        std::fs::write(path, format!("\n\n{source}")).unwrap();
        runtime.driver().record_absolute_change(path);
    }
    assert!(super::needs_reload(&runtime.installed()));
    let schedule = fast_schedule(root.path());
    let limiter = Arc::clone(&schedule.limiter);
    let held = limiter.try_acquire().unwrap();
    runtime.driver().take_reconcile_work();
    let worker = {
        let weak = Arc::downgrade(&slot);
        let runtime = Arc::clone(&runtime);
        let model = Arc::clone(&model);
        std::thread::spawn(move || {
            catch_up(
                &weak,
                epoch,
                &runtime,
                &schedule,
                FillBudget::default(),
                &mut |texts| model.embed(texts),
            )
        })
    };
    let queued = wait_for_fill_queue(&limiter);
    let work_while_held = runtime.driver().take_reconcile_work();
    drop(held);
    assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
    assert!(queued, "many unreconciled edits bypassed cold admission");
    assert_eq!(
        work_while_held.walks, 0,
        "reconciled before cold admission: {work_while_held:?}"
    );
}

/// One edited file larger than the small-preparation byte bound takes a
/// cold-build slot before the reconcile reads it, even though it is a single
/// path. A small edit still reconciles on the preparation lane; see
/// `unreconciled_reuse_only_edit_completes_with_cold_cap_held`.
#[test]
fn large_unreconciled_edit_takes_cold_slot_before_reconciling() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("large.rs");
    std::fs::write(&path, "pub fn large_edit() {}\n").unwrap();
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let model = Arc::new(Model::default());
    refresh(&runtime, &model);
    let padding = "// padding\n".repeat((SMALL_PREPARATION_BYTES as usize) / 11 + 1);
    std::fs::write(&path, format!("pub fn large_edit() {{}}\n{padding}")).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() > SMALL_PREPARATION_BYTES);
    runtime.driver().record_absolute_change(&path);
    assert!(super::needs_reload(&runtime.installed()));
    let schedule = fast_schedule(root.path());
    let limiter = Arc::clone(&schedule.limiter);
    let held = limiter.try_acquire().unwrap();
    runtime.driver().take_reconcile_work();
    let worker = {
        let weak = Arc::downgrade(&slot);
        let runtime = Arc::clone(&runtime);
        let model = Arc::clone(&model);
        std::thread::spawn(move || {
            catch_up(
                &weak,
                epoch,
                &runtime,
                &schedule,
                FillBudget::default(),
                &mut |texts| model.embed(texts),
            )
        })
    };
    let queued = wait_for_fill_queue(&limiter);
    let work_while_held = runtime.driver().take_reconcile_work();
    drop(held);
    assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
    assert_eq!(
        work_while_held.walks, 0,
        "a large edit was reconciled on the preparation lane while every cold slot was held: \
         {work_while_held:?}"
    );
    assert!(queued, "a large edit bypassed cold admission");
}

/// A fill that leaves work for another budgeted round releases its cold-build
/// slot between rounds. When another request is queued by then, the fill's next
/// round queues behind it instead of retaking the slot it just freed.
#[test]
fn multi_round_fill_yields_to_a_queued_waiter_between_rounds() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(
        root.path(),
        &[
            ("first.rs", "pub fn first_round() {}\n"),
            ("second.rs", "pub fn second_round() {}\n"),
        ],
    );
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let schedule = fast_schedule(root.path());
    let limiter = Arc::clone(&schedule.limiter);
    let model = Arc::new(Model::default());
    let mut queued_waiter = None;
    let outcome = catch_up(
        &Arc::downgrade(&slot),
        epoch,
        &runtime,
        &schedule,
        FillBudget {
            max_files: 1,
            ..FillBudget::default()
        },
        &mut |texts| {
            if queued_waiter.is_none() {
                // While the first round holds the only slot, queue another
                // maintenance request and record how many rounds had embedded
                // when it was admitted.
                let waiter_limiter = Arc::clone(&limiter);
                let waiter_model = Arc::clone(&model);
                queued_waiter = Some(std::thread::spawn(move || {
                    let permit = crate::cold_build_limiter::acquire_blocking_while_with_limiter(
                        &waiter_limiter,
                        "queued maintenance",
                        || true,
                    )
                    .expect("queued request is admitted");
                    let calls_at_admission = waiter_model.calls();
                    drop(permit);
                    calls_at_admission
                }));
                let deadline = Instant::now() + Duration::from_secs(10);
                while !limiter
                    .census()
                    .queued
                    .iter()
                    .any(|job| job.kind == "queued maintenance")
                {
                    assert!(Instant::now() < deadline, "the other request never queued");
                    std::thread::yield_now();
                }
            }
            model.embed(texts)
        },
    );
    assert!(matches!(outcome, FillOutcome::Settled));
    assert_eq!(model.calls(), 2, "each file takes its own round");
    let calls_at_admission = queued_waiter
        .expect("the first round queued another request")
        .join()
        .unwrap();
    assert_eq!(
        calls_at_admission, 1,
        "the fill retook the slot for its second round ahead of the queued request"
    );
}

fn wait_for_fill_queue(limiter: &crate::cold_build_limiter::ColdBuildLimiter) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !limiter.census().queued.is_empty() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn small_missing_vectors_wait_for_cold_slot_and_stay_pending() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), &[("file.rs", "pub fn new_vector() {}\n")]);
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let schedule = fast_schedule(root.path());
    let limiter = Arc::clone(&schedule.limiter);
    let held = limiter.try_acquire().unwrap();
    let model = Arc::new(Model::default());
    let worker = {
        let weak = Arc::downgrade(&slot);
        let runtime = Arc::clone(&runtime);
        let model = Arc::clone(&model);
        let limiter = Arc::clone(&limiter);
        std::thread::spawn(move || {
            catch_up(
                &weak,
                epoch,
                &runtime,
                &schedule,
                FillBudget::default(),
                &mut |texts| {
                    assert!(
                        !limiter.census().holders.is_empty(),
                        "model ran without admission"
                    );
                    model.embed(texts)
                },
            )
        })
    };
    let queued = wait_for_fill_queue(&limiter);
    let calls_while_held = model.calls();
    let pending_while_held = query(&runtime, "new vector").pending.len();
    let reads_while_held = runtime.plane().take_source_reads();
    drop(held);
    assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
    assert!(queued);
    assert_eq!(calls_while_held, 0, "queued fill called the model");
    assert_eq!(pending_while_held, 1, "queued fill was reported as ready");
    assert_eq!(
        reads_while_held.get(&root.path().join("file.rs")),
        Some(&1),
        "small preparation did not reach the vector miss"
    );
    assert_eq!(model.calls(), 1);
    assert!(query(&runtime, "new vector").complete());
}

#[test]
fn large_fill_size_gate_takes_cold_slot_before_source_preparation() {
    for many_files in [true, false] {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        if many_files {
            for i in 0..33 {
                std::fs::write(
                    root.path().join(format!("file_{i}.rs")),
                    "pub fn large_queue() {}\n",
                )
                .unwrap();
            }
        } else {
            std::fs::write(
                root.path().join("large.rs"),
                format!("pub fn large_input() {{}}\n{}", " ".repeat(256 * 1024)),
            )
            .unwrap();
        }
        let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
        let schedule = fast_schedule(root.path());
        let limiter = Arc::clone(&schedule.limiter);
        let held = limiter.try_acquire().unwrap();
        runtime.plane().take_source_reads();
        let worker = {
            let weak = Arc::downgrade(&slot);
            let runtime = Arc::clone(&runtime);
            std::thread::spawn(move || {
                catch_up(
                    &weak,
                    epoch,
                    &runtime,
                    &schedule,
                    FillBudget {
                        max_files: 1,
                        ..FillBudget::default()
                    },
                    &mut |texts| Model::default().embed(texts),
                )
            })
        };
        let queued = wait_for_fill_queue(&limiter);
        let reads_while_held = runtime.plane().take_source_reads();
        let pending_while_held = query(&runtime, "large").pending.len();
        drop(held);
        assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
        assert!(
            queued,
            "large fill bypassed cold admission: many_files={many_files}"
        );
        assert!(
            reads_while_held.is_empty(),
            "large fill prepared before admission: {reads_while_held:?}"
        );
        assert_eq!(pending_while_held, if many_files { 33 } else { 1 });
        assert!(query(&runtime, "large").complete());
    }
}

#[test]
fn small_preparation_bounds_reads_when_queued_file_grows() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = "pub fn small_input() {}\n";
    write_tree(root.path(), &[("file.rs", source)]);
    let (_slot, _epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let report = runtime
        .refresh_admitted(
            FillBudget::default(),
            &mut |_| panic!("moved source reached the model"),
            &mut |small| {
                assert!(small, "fixture did not reach small preparation");
                std::fs::write(root.path().join("file.rs"), " ".repeat(1024 * 1024)).unwrap();
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        (
            report.queued,
            report.moved,
            report.installed,
            report.model_calls
        ),
        (1, 1, 0, 0)
    );
    assert_eq!(report.source_reads, 1);
    assert_eq!(
        report.source_bytes,
        source.len() as u64 + 1,
        "small preparation read unbounded growth"
    );
    assert_eq!(query(&runtime, "small input").pending.len(), 1);
}

#[test]
fn fifth_small_fill_waits_for_preparation_capacity_and_stays_pending() {
    let storage: Vec<_> = (0..5).map(|_| tempfile::tempdir().unwrap()).collect();
    let roots: Vec<_> = (0..5).map(|_| tempfile::tempdir().unwrap()).collect();
    let preparation = crate::cold_build_limiter::isolated_limiter(PREPARATION_LIMIT);
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(5);
    let (release_tx, release_rx) = crossbeam_channel::bounded(5);
    let mut lanes = Vec::new();
    let mut workers = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        write_tree(
            root.path(),
            &[("file.rs", &format!("pub fn small_{i}() {{}}\n"))],
        );
        let (slot, epoch, _wake, runtime) = served_lane(storage[i].path(), root.path());
        let schedule = fast_schedule(root.path());
        let weak = Arc::downgrade(&slot);
        let worker_runtime = Arc::clone(&runtime);
        let preparation = Arc::clone(&preparation);
        let entered = entered_tx.clone();
        let release = release_rx.clone();
        workers.push(std::thread::spawn(move || {
            super::super::semantic::before_claim_for_test(move || {
                entered.send(i).unwrap();
                // Keep the permit until the main thread has observed all pending
                // work, even when fixture setup or queries are slow on Windows.
                // The test runner's overall timeout bounds a genuine hang.
                release.recv().unwrap();
            });
            catch_up_with_preparation_limiter(
                &weak,
                epoch,
                &worker_runtime,
                &schedule,
                FillBudget::default(),
                &mut |texts| Model::default().embed(texts),
                &preparation,
            )
        }));
        lanes.push((slot, runtime));
    }
    drop(entered_tx);
    let first_four: Vec<_> = (0..4)
        .map(|_| entered_rx.recv().expect("fill did not enter preparation"))
        .collect();
    let census = loop {
        let census = preparation.census();
        if !census.queued.is_empty() {
            break census;
        }
        assert!(
            workers.iter().all(|worker| !worker.is_finished()),
            "fill exited before the preparation wait was visible"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let queued = !census.queued.is_empty();
    let holders = census.holders.len();
    let early_fifth = entered_rx.try_recv().ok();
    let pending: Vec<_> = lanes
        .iter()
        .map(|(_, runtime)| query(runtime, "small").pending.len())
        .collect();
    let queued_visible = roots.iter().any(|root| {
        crate::cold_build_limiter::progress::snapshot_for_root(root.path(), 0)
            .queued
            .iter()
            .any(|job| job.kind == PREPARATION_KIND)
    });
    release_tx.send(()).unwrap();
    let fifth = entered_rx.recv().ok();
    for _ in 0..4 {
        release_tx.send(()).unwrap();
    }
    for worker in workers {
        assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
    }
    assert_eq!(first_four.len(), 4);
    assert!(queued && queued_visible, "preparation wait was not visible");
    assert_eq!(holders, 4);
    assert!(
        early_fifth.is_none(),
        "fifth fill exceeded preparation capacity"
    );
    assert!(
        fifth.is_some_and(|i| !first_four.contains(&i)),
        "fifth fill did not resume after a preparation finished"
    );
    assert_eq!(pending, vec![1; 5], "preparations were reported as filled");
    assert!(lanes
        .iter()
        .all(|(_, runtime)| query(runtime, "small").complete()));
}

/// A real HTTP backend that reserves its port even while offline, so another
/// test cannot steal the address between startup and recovery attempts.
struct RecoveringBackend {
    addr: std::net::SocketAddr,
    socket: Option<tokio::net::TcpSocket>,
    status: Arc<AtomicUsize>,
    probes: Arc<AtomicUsize>,
    texts: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl RecoveringBackend {
    fn offline() -> Self {
        let (socket, addr) = crate::semantic_index::reserved_refused_backend_for_test();
        Self {
            addr,
            socket: Some(socket),
            status: Arc::new(AtomicUsize::new(0)),
            probes: Arc::new(AtomicUsize::new(0)),
            texts: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    fn config(&self) -> crate::config::SemanticBackendConfig {
        crate::config::SemanticBackendConfig {
            backend: crate::config::SemanticBackend::OpenAiCompatible,
            base_url: Some(format!("http://{}", self.addr)),
            model: "retry-test".into(),
            timeout_ms: 1000,
            ..Default::default()
        }
    }

    fn start(&mut self, status: u16) {
        use std::io::{Read, Write};
        self.status.store(usize::from(status), Ordering::SeqCst);
        if self.worker.is_some() {
            return;
        }
        let listener = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap()
            .block_on(async {
                self.socket
                    .take()
                    .expect("reserved backend socket")
                    .listen(128)?
                    .into_std()
            })
            .unwrap();
        listener.set_nonblocking(true).unwrap();
        let probes = Arc::clone(&self.probes);
        let texts = Arc::clone(&self.texts);
        let status = Arc::clone(&self.status);
        let stop = Arc::clone(&self.stop);
        self.worker = Some(std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("accept test backend connection: {error}"),
                };
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                // Windows inherits the listener's nonblocking mode on accept.
                // Use blocking I/O so HTTP reads wait for request bytes.
                stream.set_nonblocking(false).unwrap();
                let response_status = status.load(Ordering::SeqCst) as u16;
                if response_status == 0 {
                    // Keep the port reserved between retries, but close requests
                    // immediately while the test backend is offline.
                    continue;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0; 4096];
                let (start, length) = loop {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let length: usize = String::from_utf8_lossy(&bytes[..end])
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            break (end + 4, length);
                        }
                    }
                };
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes[start..start + length]).unwrap();
                let inputs = body["input"].as_array().unwrap();
                let data: Vec<_> = inputs
                    .iter()
                    .enumerate()
                    .map(|(index, input)| {
                        let text = input.as_str().unwrap();
                        if text == "semantic index fingerprint probe" {
                            probes.fetch_add(1, Ordering::SeqCst);
                        } else {
                            texts.fetch_add(1, Ordering::SeqCst);
                        }
                        serde_json::json!({"index": index, "embedding": vector(text)})
                    })
                    .collect();
                let body = if response_status == 200 {
                    serde_json::json!({"data": data}).to_string()
                } else {
                    "wrong model".to_string()
                };
                write!(stream, "HTTP/1.1 {response_status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        }));
    }

    fn shutdown(&mut self) {
        self.status.store(0, Ordering::SeqCst);
    }
}

impl Drop for RecoveringBackend {
    fn drop(&mut self) {
        self.shutdown();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

#[test]
fn offline_backend_port_cannot_be_stolen_before_start() {
    let mut backend = RecoveringBackend::offline();
    let refusal = std::net::TcpStream::connect_timeout(&backend.addr, Duration::from_millis(100))
        .unwrap_err();
    // A bound, non-listening socket can refuse a connection or leave its SYN
    // unanswered (as on Darwin). Neither outcome makes the backend available.
    assert!(
        matches!(
            refusal.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
        ),
        "unexpected offline connection error: {refusal}"
    );
    let stolen = std::net::TcpListener::bind(backend.addr);
    if let Ok(stolen) = stolen {
        let start = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            backend.start(503);
        }));
        drop(stolen);
        let panic = start.expect_err("a competing listener must block backend startup");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(
            message.contains("AddrInUse"),
            "unexpected startup panic: {message}"
        );
        panic!("the offline backend port was stolen before start");
    }
    let error = stolen.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    backend.start(503);
    backend.shutdown();
}

#[test]
fn transient_fingerprint_failure_recovers_without_edit_or_rebind() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let mut backend = RecoveringBackend::offline();
    // A previous session already stored every vector. Retrying identity must
    // reuse these vectors, not re-embed the checkout when the backend returns.
    backend.start(200);
    let semantic = backend.config();
    let mut model = crate::semantic_index::EmbeddingModel::from_config(&semantic).unwrap();
    let fingerprint = model.fingerprint(&semantic).unwrap();
    let seed = CheckoutSemantic::new(
        &crate::root_cache::StoragePaths::same(storage.path()),
        "family",
        "scope",
        root.path(),
        SemanticProducer::current(fingerprint.as_string(), fingerprint.embed_text_caps),
        Weak::new(),
    )
    .unwrap();
    seed.load().unwrap();
    seed.refresh(FillBudget::default(), &mut |texts| model.embed(texts))
        .unwrap();
    let seed_work = seed.driver().take_reconcile_work();
    let stored_texts = backend.texts.load(Ordering::SeqCst);
    drop(seed);
    drop(model);
    backend.shutdown();
    let observer = FillObserver::new(root.path().to_path_buf());
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let status = Arc::new(RwLock::new(crate::context::SemanticIndexStatus::Disabled));
    let drivers = InstalledDriver::default();
    let config = WorkerConfig {
        root: root.path().to_path_buf(),
        storage: crate::root_cache::StoragePaths::same(storage.path()),
        family: "family".into(),
        scope: "scope".into(),
        semantic: backend.config(),
        status: Arc::clone(&status),
        drivers: drivers.clone(),
        schedule: fast_schedule(root.path()),
    };
    let weak = Arc::downgrade(&slot);
    let worker = std::thread::spawn(move || run_worker(weak, epoch, wake, config));
    assert_eq!(observer.next(), "unavailable");
    assert!(
        matches!(&*status.read().unwrap(), crate::context::SemanticIndexStatus::Failed(reason) if reason.contains("next retry due") && reason.contains("embedding model"))
    );
    // Observe worker transitions, not retry latency: recovery also loads the
    // stored view, and slow I/O must not be mistaken for a latched retry. These
    // event waits have only a hang guard; no edit, wake or rebind drives recovery.
    let hang_guard = Duration::from_secs(60);
    // A second refused connection proves a timer fires without any wake.
    let retried = observer.rx.recv_timeout(hang_guard).ok();
    if retried != Some("unavailable") {
        slot.clear();
        worker.join().unwrap();
        panic!("the failed fingerprint never retried without a wake: {retried:?}");
    }
    assert!(
        drivers.get().is_none(),
        "a failed probe must not walk the checkout"
    );
    backend.start(200);
    let deadline = Instant::now() + hang_guard;
    let recovered = loop {
        match observer.rx.recv_deadline(deadline) {
            Ok("unavailable") => continue,
            event => break event.ok(),
        }
    };
    let runtime = slot.runtime();
    slot.clear();
    worker.join().unwrap();
    assert_eq!(
        recovered,
        Some("settled"),
        "the failed fingerprint latched the semantic lane"
    );
    assert!(matches!(
        *status.read().unwrap(),
        crate::context::SemanticIndexStatus::Ready { .. }
    ));
    assert_eq!(
        backend.probes.load(Ordering::SeqCst),
        2,
        "one seed probe and one recovery probe"
    );
    assert_eq!(
        backend.texts.load(Ordering::SeqCst),
        stored_texts,
        "recovery re-embedded stored sources"
    );
    let runtime = runtime.unwrap();
    assert!(query(&runtime, "alpha").complete());
    assert!(
        runtime.plane().take_source_reads().is_empty(),
        "recovery fill read sources whose vectors were already stored"
    );
    let work = runtime.driver().take_reconcile_work();
    assert_eq!(work.walks, 1, "failed probes re-walked the checkout");
    assert!(
        work.reads <= seed_work.reads,
        "recovery re-read unchanged files: {work:?}"
    );
}

#[test]
fn permanent_fingerprint_failure_does_not_retry() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let mut backend = RecoveringBackend::offline();
    backend.start(400);
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let status = Arc::new(RwLock::new(crate::context::SemanticIndexStatus::Disabled));
    let config = WorkerConfig {
        root: root.path().to_path_buf(),
        storage: crate::root_cache::StoragePaths::same(storage.path()),
        family: "family".into(),
        scope: "scope".into(),
        semantic: backend.config(),
        status: Arc::clone(&status),
        drivers: InstalledDriver::default(),
        schedule: fast_schedule(root.path()),
    };
    let (done, finished) = crossbeam_channel::bounded(1);
    let weak = Arc::downgrade(&slot);
    let worker = std::thread::spawn(move || {
        run_worker(weak, epoch, wake, config);
        done.send(()).unwrap();
    });
    let stopped = finished.recv_timeout(Duration::from_secs(5)).is_ok();
    let state = slot.state();
    let armed = lock(&slot.retry).is_some();
    slot.clear();
    worker.join().unwrap();
    assert!(stopped, "a permanent failure kept the retry worker alive");
    assert!(!armed, "a permanent failure armed a retry");
    assert!(
        matches!(state, Some(CheckoutSemanticState::Unavailable(reason)) if reason.contains("wrong model") && !reason.contains("next retry"))
    );
    assert!(
        matches!(&*status.read().unwrap(), crate::context::SemanticIndexStatus::Failed(reason) if reason.contains("wrong model"))
    );
    assert_eq!(backend.probes.load(Ordering::SeqCst), 1);
}

#[test]
fn view_fingerprint_retry_sends_one_probe_even_for_server_errors() {
    let mut backend = RecoveringBackend::offline();
    backend.start(503);
    let config = backend.config();
    let mut model = crate::semantic_index::EmbeddingModel::from_config(&config).unwrap();
    let error = model.fingerprint_for_view(&config).unwrap_err();
    assert!(
        crate::semantic_index::embedding_failure_is_transient(&error),
        "{error}"
    );
    assert_eq!(
        backend.probes.load(Ordering::SeqCst),
        1,
        "one scheduled attempt sent multiple probes"
    );
    backend.shutdown();
    backend.start(200);
    model.fingerprint_for_view(&config).unwrap();
    assert_eq!(backend.probes.load(Ordering::SeqCst), 2);
    assert_eq!(backend.texts.load(Ordering::SeqCst), 0);
}

#[test]
fn query_retry_is_rate_limited_and_only_advances_transient_deadlines() {
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let weak = Arc::downgrade(&slot);
    assert!(!slot.request_retry_at(Instant::now()));
    schedule_retry(
        &weak,
        epoch,
        "[transient] offline",
        Duration::from_secs(300),
        Duration::from_secs(5),
    );
    let allowed = lock(&slot.retry).as_ref().unwrap().query_allowed_at;
    assert!(!slot.request_retry_at(allowed - Duration::from_nanos(1)));
    assert!(slot.request_retry_at(allowed));
    assert!(
        !slot.request_retry_at(allowed),
        "one query burst bypassed the limiter"
    );
    assert_eq!(lock(&slot.retry).as_ref().unwrap().due, allowed);
    clear_retry(&weak, epoch);
    assert!(!slot.request_retry_at(allowed + Duration::from_secs(10)));
    slot.clear();
    assert!(!wait_retry(&weak, epoch, &wake));
}

#[test]
fn semantic_query_status_sample_expedites_retry_without_a_loaded_runtime() {
    let storage = tempfile::tempdir().unwrap();
    let ctx = crate::context::AppContext::new(
        crate::context::default_language_provider_factory(),
        crate::config::Config {
            storage_dir: Some(storage.path().to_path_buf()),
            ..Default::default()
        },
    );
    let slot = ctx.checkout_semantic();
    let (epoch, wake) = slot.begin();
    let reason = "semantic: unavailable: embedding model offline".to_owned();
    *ctx.semantic_index_status().write().unwrap() =
        crate::context::SemanticIndexStatus::Failed(reason.clone());
    slot.fail(epoch, reason);
    // Advance only the rate-limit clock, without sleeping or contacting a
    // backend. The query must consume this allowance through the real context.
    let now = Instant::now();
    *lock(&slot.retry) = Some(RetryDeadline {
        due: now + Duration::from_secs(300),
        query_allowed_at: now - Duration::from_secs(1),
    });
    assert!(slot.runtime().is_none());
    assert!(matches!(
        *ctx.semantic_index_status().read().unwrap(),
        crate::context::SemanticIndexStatus::Failed(_)
    ));
    assert_eq!(
        wake.try_recv(),
        Ok(()),
        "query readiness did not wake the failed lane"
    );
    ctx.semantic_index_status();
    assert_eq!(
        wake.try_recv(),
        Err(crossbeam_channel::TryRecvError::Empty),
        "query burst bypassed the rate limit"
    );
    slot.clear();
}

#[test]
fn transient_fill_recovery_does_not_reread_or_reembed_stored_content() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, wake, runtime) = served_lane(storage.path(), root.path());
    let model = Arc::new(Model::default());
    refresh(&runtime, &model);
    let stored_texts = model.texts();
    let target = root.path().join("src/gamma.rs");
    std::fs::write(&target, "pub fn new_gamma() {}\n").unwrap();
    runtime.driver().record_absolute_change(&target);
    let observer = FillObserver::new(root.path().to_path_buf());
    let online = Arc::new(AtomicBool::new(false));
    let status = Arc::new(RwLock::new(crate::context::SemanticIndexStatus::ready()));
    let worker = {
        let weak = Arc::downgrade(&slot);
        let root = root.path().to_path_buf();
        let online = Arc::clone(&online);
        let model = Arc::clone(&model);
        let status = Arc::clone(&status);
        std::thread::spawn(move || {
            serve_fills_with_status(
                &weak,
                epoch,
                &wake,
                &fast_schedule(&root),
                Some(&status),
                &mut |texts| {
                    if !online.load(Ordering::SeqCst) {
                        Err("[transient] embedding backend unreachable".into())
                    } else {
                        model.embed(texts)
                    }
                },
            );
        })
    };
    assert_eq!(observer.next(), "retry");
    assert!(
        matches!(&*status.read().unwrap(), crate::context::SemanticIndexStatus::Failed(reason) if reason.contains("next retry due"))
    );
    assert!(query(&runtime, "alpha").unavailable.is_some());
    runtime.driver().take_reconcile_work();
    runtime.plane().take_source_reads();
    online.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    let settled = loop {
        match observer.rx.recv_deadline(deadline) {
            Ok("retry") => continue,
            event => break event.ok(),
        }
    };
    slot.clear();
    worker.join().unwrap();
    assert_eq!(settled, Some("settled"));
    let work = runtime.driver().take_reconcile_work();
    assert_eq!(
        (work.walks, work.reads),
        (0, 0),
        "retry repeated source work: {work:?}"
    );
    let reads = runtime.plane().take_source_reads();
    assert_eq!(
        reads.keys().collect::<Vec<_>>(),
        vec![&target],
        "retry read an unchanged, already-embedded source: {reads:?}"
    );
    assert_eq!(
        model.texts() - stored_texts,
        1,
        "retry re-embedded unchanged sources"
    );
    assert!(query(&runtime, "gamma").complete());
    assert!(matches!(
        *status.read().unwrap(),
        crate::context::SemanticIndexStatus::Ready { .. }
    ));
}

#[test]
fn permanent_fill_failure_preserves_reason_without_retry() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, wake, runtime) = served_lane(storage.path(), root.path());
    let observer = FillObserver::new(root.path().to_path_buf());
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = {
        let weak = Arc::downgrade(&slot);
        let root = root.path().to_path_buf();
        let calls = Arc::clone(&calls);
        std::thread::spawn(move || {
            serve_fills(&weak, epoch, &wake, &fast_schedule(&root), &mut |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Err("embedding dimension mismatch".into())
            });
        })
    };
    let event = observer.next();
    let armed = lock(&slot.retry).is_some();
    slot.clear();
    worker.join().unwrap();
    assert_eq!(event, "settled");
    assert!(!armed, "dimension mismatch armed a retry");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.unavailable_reason().as_deref(),
        Some("embedding dimension mismatch")
    );
}

#[test]
fn live_view_fill_progress_is_visible_before_each_model_call() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let schedule = fast_schedule(root.path());
    let mut calls = 0;
    let mut saw_finished_file = false;
    let outcome = catch_up(
        &Arc::downgrade(&slot),
        epoch,
        &runtime,
        &schedule,
        FillBudget {
            max_files: 1,
            max_batch: 1,
            ..FillBudget::default()
        },
        &mut |texts| {
            calls += 1;
            let snapshot = crate::cold_build_limiter::progress::snapshot_for_root(root.path(), 0);
            let job = snapshot
                .running
                .iter()
                .find(|job| job.root == root.path().to_string_lossy())
                .unwrap();
            assert_eq!(job.kind, "semantic view fill");
            assert_eq!(job.phase, "embedding");
            assert_eq!(job.total, Some(3));
            if calls > 1 {
                assert_eq!(job.chunks_embedded, Some((calls - 1) as u64));
            }
            saw_finished_file |= job.done > 0;
            Ok(texts.iter().map(|text| vector(text)).collect())
        },
    );
    assert!(matches!(outcome, FillOutcome::Settled));
    assert!(saw_finished_file);
    assert!(
        !crate::cold_build_limiter::progress::snapshot_for_root(root.path(), 0)
            .running
            .iter()
            .any(|job| job.root == root.path().to_string_lossy())
    );
}

struct DelayedLocalProvider {
    delay: Duration,
}

impl LocalEmbeddingProvider for DelayedLocalProvider {
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        std::thread::sleep(self.delay);
        Ok(texts.iter().map(|text| vector(text)).collect())
    }
}

#[test]
fn semantic_view_fill_summary_reports_backend_embedding_timing() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let schedule = fast_schedule(root.path());
    let delay = Duration::from_millis(30);
    let mut model = SemanticEmbeddingModel::from_local_provider_for_test(
        Box::new(DelayedLocalProvider { delay }),
        root.path().to_path_buf(),
    );
    let mut calls = 0;

    let (outcome, lines) = crate::logging::capture_log_lines(|| {
        catch_up(
            &Arc::downgrade(&slot),
            epoch,
            &runtime,
            &schedule,
            FillBudget {
                max_files: 1,
                max_batch: 1,
                ..FillBudget::default()
            },
            &mut |texts| {
                calls += 1;
                model.embed(texts)
            },
        )
    });

    assert!(matches!(outcome, FillOutcome::Settled));
    assert!(calls > 0, "{lines:?}");
    let summary = lines
        .iter()
        .find(|line| line.contains("INFO semantic view fill"))
        .unwrap_or_else(|| panic!("missing view fill summary in {lines:?}"));
    for field in [
        "elapsed_ms=",
        "embed_ms=",
        "embed_max_ms=",
        "admission_wait_ms=",
        "mutex_wait_ms=",
        "reconcile_ms=",
        "queue_setup_ms=",
        "read_hash_ms=",
        "file_lookup_ms=",
        "chunk_ms=",
        "chunk_wait_ms=",
        "vector_lookup_ms=",
        "store_write_ms=",
        "shared_wait_ms=",
        "fold_ms=",
        "source_reads=",
        "source_bytes=",
    ] {
        assert!(summary.contains(field), "{summary}");
    }
    let field = |name: &str| -> u128 {
        summary
            .split_whitespace()
            .find_map(|part| part.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("missing {name} in {summary}"))
            .parse()
            .unwrap()
    };
    assert!(field("embed_ms") >= delay.as_millis(), "{summary}");
    assert!(field("embed_max_ms") >= delay.as_millis(), "{summary}");
    assert!(field("elapsed_ms") >= field("embed_ms"), "{summary}");
}

/// A lane whose checkout is loaded and served, as the worker leaves it.
fn served_lane(
    storage: &Path,
    root: &Path,
) -> (
    Arc<CheckoutSemanticSlot>,
    u64,
    crossbeam_channel::Receiver<()>,
    Arc<CheckoutSemantic>,
) {
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let runtime = Arc::new(
        CheckoutSemantic::new(
            &crate::root_cache::StoragePaths::same(storage),
            "family",
            "scope",
            root,
            producer(),
            Arc::downgrade(&slot),
        )
        .unwrap(),
    );
    runtime.load().unwrap();
    assert!(slot.install(epoch, Arc::clone(&runtime)));
    (slot, epoch, wake, runtime)
}

/// A fill that fails because the embedding backend is down is retried on a
/// backoff: with no edit, wake or reconfigure, the view becomes complete once
/// the backend answers again.
#[test]
fn transient_embed_errors_retry_until_complete_without_a_wake() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, wake, runtime) = served_lane(storage.path(), root.path());
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = {
        let weak = Arc::downgrade(&slot);
        let calls = Arc::clone(&calls);
        let root = root.path().to_path_buf();
        std::thread::spawn(move || {
            let schedule = fast_schedule(&root);
            serve_fills(&weak, epoch, &wake, &schedule, &mut |texts: Vec<String>| {
                // The backend is unreachable for the first three calls.
                if calls.fetch_add(1, Ordering::SeqCst) < 3 {
                    return Err("[transient] embedding backend unreachable".to_string());
                }
                Ok(texts.iter().map(|text| vector(text)).collect())
            });
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let complete = loop {
        if query(&runtime, "alpha").complete() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    slot.clear();
    worker.join().unwrap();
    assert!(
        complete,
        "a transient embed error left the view partial with no retry"
    );
    assert!(calls.load(Ordering::SeqCst) >= 4);
}

/// A superseded runtime of the same view (a lane restarted with the same
/// producer) must not release the state of the runtime that replaced it.
#[test]
fn dropping_a_superseded_runtime_keeps_the_replacements_view() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let old = open(storage.path(), "scope", root.path());
    let new = open(storage.path(), "scope", root.path());
    assert!(Arc::ptr_eq(old.plane(), new.plane()));
    refresh(&new, &Model::default());
    drop(old);
    let answer = new
        .search(&vector("alpha"), 10, &|_| true)
        .expect("the replacement lost its resident generation");
    assert!(
        answer.complete(),
        "the replacement lost its fills: {answer:?}"
    );
}

/// Every `CheckoutSemantic::new` holds its view once and every drop releases
/// it once, keyed by the plane's never-reused id rather than its address.
#[test]
fn live_view_holders_match_runtimes_and_planes_have_unique_ids() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let first = open(storage.path(), "holders", root.path());
    let plane = Arc::clone(first.plane());
    let access = first.access().clone();
    assert_eq!(live_holders(&plane, &access), 1);
    let second = open(storage.path(), "holders", root.path());
    assert_eq!(live_holders(&plane, &access), 2);
    drop(first);
    assert_eq!(live_holders(&plane, &access), 1);
    drop(second);
    assert_eq!(live_holders(&plane, &access), 0);

    let id = plane.id();
    drop(plane);
    // A plane built after the previous one was freed, possibly at the same
    // address, still gets an id of its own.
    let replacement = shared_plane(storage.path(), producer());
    assert_ne!(replacement.id(), id);
}

#[test]
fn only_busy_and_checkout_races_are_transient_load_errors() {
    for transient in [
        super::super::first_load::CHECKOUT_CHANGED_DURING_RECONCILE,
        super::super::first_load::CHECKOUT_WRITE_ACTIVE,
        super::super::first_load::CHECKOUT_CHANGED_BEFORE_INSTALL,
        "trigram plane: database is locked",
    ] {
        assert!(is_transient_reason(transient), "{transient}");
    }
    for permanent in [
        "semantic plane: file is not a database",
        "trigram plane: Permission denied (os error 13)",
        "semantic plane: semantic producer mismatch",
        "unsupported persisted format: written by a newer build",
    ] {
        assert!(!is_transient_reason(permanent), "{permanent}");
    }
}

/// A refresh error no timed retry would fix (here the view store cannot be
/// written) becomes the checkout's unavailable reason after one attempt;
/// the worker does not retry it on the backoff. An embedding failure, by contrast, is
/// retried (`transient_embed_errors_retry_until_complete_without_a_wake`).
#[cfg(unix)]
#[test]
fn store_errors_name_the_gap_without_a_retry_loop() {
    use std::os::unix::fs::PermissionsExt;
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, wake, runtime) = served_lane(storage.path(), root.path());
    // An edit makes the next refresh reload, which publishes into the view
    // directory; an owner-only read-only directory refuses that publication.
    // Public read/execute bits would be repaired when the store is reopened.
    std::fs::write(root.path().join("src/gamma.rs"), "pub fn gamma_edit() {}\n").unwrap();
    runtime
        .driver()
        .record_absolute_change(&root.path().join("src/gamma.rs"));
    let view_dir = crate::views::registry::view_dir(storage.path(), "scope").unwrap();
    let original = std::fs::metadata(&view_dir).unwrap().permissions();
    std::fs::set_permissions(&view_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let worker = {
        let weak = Arc::downgrade(&slot);
        let root = root.path().to_path_buf();
        std::thread::spawn(move || {
            let schedule = fast_schedule(&root);
            serve_fills(&weak, epoch, &wake, &schedule, &mut |texts: Vec<String>| {
                Ok(texts.iter().map(|text| vector(text)).collect())
            });
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while runtime.unavailable_reason().is_none() {
        assert!(Instant::now() < deadline, "the store error was never named");
        std::thread::sleep(Duration::from_millis(5));
    }
    // The backoff (20 ms doubling to 80 ms) would allow several refresh
    // attempts in this wait if the error were treated as transient.
    std::thread::sleep(Duration::from_millis(500));
    let attempts = runtime.refresh_attempts();
    let answer = query(&runtime, "gamma");
    std::fs::set_permissions(&view_dir, original).unwrap();
    slot.clear();
    worker.join().unwrap();
    assert_eq!(attempts, 1, "a store error was retried on the backoff");
    assert!(!answer.complete());
    assert!(
        answer
            .unavailable
            .as_deref()
            .is_some_and(|reason| reason.contains("Permission denied")),
        "{answer:?}"
    );
}

/// A worker superseded by a lane restart (here a model change) is held just
/// before its final publication while the new lane publishes; when released,
/// its publication is refused and the view still names the new producer. No
/// wait for the old worker is involved: the fence alone keeps the old
/// producer's manifest from ever replacing the new one.
#[test]
fn superseded_lane_cannot_publish_over_its_replacement() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let lane = |model: &str, slot: &Arc<CheckoutSemanticSlot>, epoch: u64| {
        let runtime = CheckoutSemantic::new(
            &crate::root_cache::StoragePaths::same(storage.path()),
            "family",
            "scope",
            root.path(),
            SemanticProducer::current(model, EmbedTextCaps::default()),
            Arc::downgrade(slot),
        )
        .unwrap();
        runtime
            .driver()
            .set_publish_fence(Arc::new(LaneFence::new(Arc::downgrade(slot), epoch)));
        Arc::new(runtime)
    };
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (old_epoch, _old_wake) = slot.begin();
    let old = lane("old-model", &slot, old_epoch);
    old.load().unwrap();

    // The old worker's final fold, held at a barrier before it publishes.
    let (release, barrier) = crossbeam_channel::bounded::<()>(0);
    let final_fold = {
        let old = Arc::clone(&old);
        std::thread::spawn(move || {
            barrier.recv().unwrap();
            old.load()
        })
    };
    let (new_epoch, _new_wake) = slot.begin();
    let new = lane("new-model", &slot, new_epoch);
    new.load().unwrap();
    release.send(()).unwrap();
    let refused = final_fold.join().unwrap();

    let error = refused.expect_err("a superseded lane published its view");
    assert!(
        error.contains(super::super::first_load::SUPERSEDED_PUBLISH),
        "{error}"
    );
    let registry = FamilyRegistry::open(storage.path(), "family").unwrap();
    let view = registry
        .register_view_with_storage(registry.storage(), "scope", root.path())
        .unwrap();
    let store = view.view_store().unwrap();
    let current = store.current_generation().unwrap().unwrap();
    let manifest = store.load_manifest_v2(&current).unwrap();
    assert_eq!(
        manifest.header().producers.semantic.as_deref(),
        Some(new.plane().semantic_producer().id().as_str()),
        "the view names the superseded producer"
    );
}

/// An AFT write under the root wakes the lane's worker; one elsewhere does not.
#[test]
fn aft_writes_wake_only_their_own_lane() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    write_tree(&root_path, FILES);
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let runtime = CheckoutSemantic::new(
        &crate::root_cache::StoragePaths::same(storage.path()),
        "family",
        "scope",
        &root_path,
        producer(),
        Arc::downgrade(&slot),
    )
    .unwrap();
    assert!(slot.install(epoch, Arc::new(runtime)));
    {
        let _intent = crate::views::intent::record_paths([other.path().join("x.rs").as_path()]);
    }
    assert!(wake.try_recv().is_err());
    {
        let _intent =
            crate::views::intent::record_paths([root_path.join("src/alpha.rs").as_path()]);
    }
    assert!(wake.try_recv().is_ok());
    slot.clear();
    assert!(slot.runtime().is_none());
    assert!(!slot.install(epoch, Arc::new(open(storage.path(), "scope", &root_path))));
}

/// Resident memory of this plane against the legacy index for the same
/// checkout. Run on a real tree with
/// `AFT_SEMANTIC_MEMORY_ROOT=<checkout> cargo test -p agent-file-tools --lib
/// semantic_runtime::tests::report_resident_memory -- --ignored --nocapture`.
#[test]
#[ignore = "measurement, run by hand on a real checkout"]
fn report_resident_memory() {
    let root = std::env::var_os("AFT_SEMANTIC_MEMORY_ROOT")
        .map(PathBuf::from)
        .expect("AFT_SEMANTIC_MEMORY_ROOT");
    let root = std::fs::canonicalize(root).unwrap();
    // Vectors as wide as the default local model's (384 floats), so the
    // byte counts match what a real embedding run would hold.
    let wide = |text: &str| -> Vec<f32> {
        let seed = blake3::hash(text.as_bytes());
        (0..384)
            .map(|index| f32::from(seed.as_bytes()[index % 32]) / 255.0)
            .collect()
    };
    let storage = tempfile::tempdir().unwrap();
    let runtime = open(storage.path(), "memory", &root);
    let started = Instant::now();
    let mut texts = 0usize;
    loop {
        let report = runtime
            .refresh(
                FillBudget {
                    max_files: 100_000,
                    ..FillBudget::default()
                },
                &mut |batch: Vec<String>| {
                    texts += batch.len();
                    Ok(batch.iter().map(|text| wide(text)).collect())
                },
            )
            .unwrap();
        if report.deferred == 0 {
            break;
        }
    }
    let fill_ms = started.elapsed().as_millis();
    let _ = runtime.search(&wide("query"), 10, &|_| true).unwrap();
    let memory = runtime.memory();
    let files = ConfiguredMembershipWalker.files(&root).unwrap();
    let legacy = SemanticIndex::build(
        &root,
        &files,
        &mut |batch: Vec<String>| Ok(batch.iter().map(|text| wide(text)).collect()),
        64,
    )
    .unwrap();
    println!(
        "semantic memory root={} texts={} fill_ms={} view_family_arena_bytes={} view_private_bytes={} legacy_index_bytes={}",
        root.display(),
        texts,
        fill_ms,
        memory.family_arena,
        memory.private,
        legacy.estimated_memory().estimated_bytes.unwrap_or(0)
    );
}
